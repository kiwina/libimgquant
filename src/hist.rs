//! Color histogram.
//!
//! Colors are counted in open-addressing hash tables (Knuth, TAOCP vol. 3, 2nd ed.,
//! section 6.4: multiplicative hashing with linear probing) instead of 2.4.1's chained
//! table. The histogram items are then put in the order of 2.4.1's table: by bucket
//! (`color % hash_size`), and by first appearance within a bucket. That order affects which
//! colors median cut picks, so keeping it keeps the palettes of the 2.4.1 port.
//!
//! Parallel work is split by partition, a fixed range of 2.4.1's buckets: a color belongs to
//! one partition, and concatenating the partitions gives the bucket order.
//!
//! - [`Profile::Compat`] gives the histogram of 2.4.1 bit for bit: weights of each color are
//!   summed as `f32` in pixel order (`f32` sums depend on the order of additions), and
//!   2.4.1's color limit check is emulated. Large images are split into per-partition runs
//!   of pixels in parallel, and each partition is then counted in pixel order.
//! - [`Profile::Improved`] counts weights as exact integers, which can be added in any
//!   grouping, so fixed bands of pixels are counted in parallel and then merged per
//!   partition. The color limit is a plain limit on the number of colors.
//!
//! Both give the same result for any number of threads, and with or without the `threads`
//! feature.

use crate::RGBA;
use crate::attr::{Attributes, Profile};
use crate::error::Error;
use crate::image::Image;
use crate::pal::{ColorLut, FPixel, cmax, cmin};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HistItem {
    pub color: FPixel,
    /// Perceptual weight changed to tweak how median cut selects colors.
    pub adjusted_weight: f32,
    /// Number of pixels weighted by importance of different areas of the picture.
    pub perceptual_weight: f32,
    /// Changes every time a histogram subset is sorted.
    pub color_weight: f32,
    pub sort_value: u32,
    pub likely_colormap_index: u8,
}

pub(crate) struct HistogramInternal {
    pub items: Vec<HistItem>,
    pub total_perceptual_weight: f64,
    pub ignorebits: u32,
}

/// Number of buckets of 2.4.1's hash table, which defines the histogram order.
fn hash_size(maxcolors: u32, surface: u64, ignorebits: u32) -> u32 {
    let estimated_colors = cmin(
        u64::from(maxcolors),
        surface / u64::from(ignorebits + if surface > 512 * 512 { 5 } else { 4 }),
    );
    if estimated_colors < 66000 {
        6673
    } else if estimated_colors < 200_000 {
        12011
    } else {
        24019
    }
}

/// Packs a pixel into the 32-bit histogram key, posterized by `ignorebits`.
#[derive(Clone, Copy)]
struct Posterize {
    ignorebits: u32,
    mask: u32,
    high_mask: u32,
}

impl Posterize {
    fn new(ignorebits: u32) -> Self {
        let channel_mask = 255u32 >> ignorebits << ignorebits;
        let channel_hmask = (255u32 >> ignorebits) ^ 0xFF;
        Self {
            ignorebits,
            mask: u32::from_ne_bytes([channel_mask as u8; 4]),
            high_mask: u32::from_ne_bytes([channel_hmask as u8; 4]),
        }
    }

    #[inline(always)]
    fn color(self, px: RGBA) -> u32 {
        if px.a == 0 {
            // "dirty alpha" has different RGBA values that end up being the same fully transparent color
            return 0;
        }
        // packed little-endian, so hashing is identical on every platform
        let l = u32::from_le_bytes([px.r, px.g, px.b, px.a]);
        // mask posterizes all 4 channels in one go
        (l & self.mask) | ((l & self.high_mask) >> (8 - self.ignorebits))
    }
}

#[derive(Clone, Copy, Default)]
struct Slot<W> {
    color: u32,
    /// 1 + index of the color in first-seen order, or 0 for an empty slot.
    order: u32,
    weight: W,
}

/// Open-addressing table of colors and their weights. Colors are stored in the slots,
/// so a lookup usually touches one cache line.
struct ColorTable<W> {
    /// A power of two, at most half full.
    slots: Vec<Slot<W>>,
    len: usize,
    /// 64 minus log2 of the number of slots.
    shift: u32,
}

impl<W: Copy + Default + std::ops::AddAssign> ColorTable<W> {
    fn with_capacity(colors: usize) -> Self {
        let slots = (colors.max(8) * 2).next_power_of_two();
        Self {
            slots: vec![Slot::default(); slots],
            len: 0,
            shift: 64 - slots.trailing_zeros(),
        }
    }

    /// Fibonacci hashing: the top bits of the product depend on all bits of the color.
    #[inline(always)]
    fn home_slot(&self, color: u32) -> usize {
        (u64::from(color).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize
    }

    /// Returns the slot of `color`, and true if the color is new (it is inserted with zero weight).
    #[inline(always)]
    fn find_or_insert(&mut self, color: u32) -> (usize, bool) {
        let mask = self.slots.len() - 1;
        let mut i = self.home_slot(color);
        loop {
            let s = &self.slots[i];
            if s.order == 0 {
                break;
            }
            if s.color == color {
                return (i, false);
            }
            i = (i + 1) & mask;
        }
        self.len += 1;
        self.slots[i] = Slot {
            color,
            order: self.len as u32,
            weight: W::default(),
        };
        if self.len * 2 > self.slots.len() {
            self.grow();
            return (self.find_or_insert(color).0, true);
        }
        (i, true)
    }

    #[inline(always)]
    fn add(&mut self, color: u32, weight: W) {
        let (i, _) = self.find_or_insert(color);
        self.slots[i].weight += weight;
    }

    fn grow(&mut self) {
        let new_slots = vec![Slot::default(); self.slots.len() * 2];
        let old = std::mem::replace(&mut self.slots, new_slots);
        self.shift -= 1;
        let mask = self.slots.len() - 1;
        for s in old.into_iter().filter(|s| s.order != 0) {
            let mut i = self.home_slot(s.color);
            while self.slots[i].order != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = s;
        }
    }

    /// Colors and weights in first-seen order.
    fn into_entries(self) -> Vec<(u32, W)> {
        let mut entries = vec![(0, W::default()); self.len];
        for s in self.slots.into_iter().filter(|s| s.order != 0) {
            entries[s.order as usize - 1] = (s.color, s.weight);
        }
        entries
    }
}

/// Adds weights to a [`ColorTable`], keeping the weight of the current run of equal
/// colors in a register. The weights are added in the same order as one by one, so `f32`
/// sums are identical.
struct RunAdder<W> {
    color: u32,
    /// `usize::MAX` before the first color.
    slot: usize,
    weight: W,
}

impl<W: Copy + Default + std::ops::AddAssign> RunAdder<W> {
    fn new() -> Self {
        Self {
            color: 0,
            slot: usize::MAX,
            weight: W::default(),
        }
    }

    /// Adds `weight` to `color`. Returns true if the color is new.
    #[inline(always)]
    fn add(&mut self, table: &mut ColorTable<W>, color: u32, weight: W) -> bool {
        let mut new = false;
        if color != self.color || self.slot == usize::MAX {
            self.flush(table);
            let (slot, is_new) = table.find_or_insert(color);
            self.color = color;
            self.slot = slot;
            self.weight = table.slots[slot].weight;
            new = is_new;
        }
        self.weight += weight;
        new
    }

    /// Stores the weight of the current run in the table.
    fn flush(&self, table: &mut ColorTable<W>) {
        if let Some(s) = table.slots.get_mut(self.slot) {
            s.weight = self.weight;
        }
    }
}

/// Stable counting sort of `entries` by `color % hash_size`, for buckets
/// `first_bucket..first_bucket + bucket_len.len()`. `bucket_len` holds the number of entries
/// per bucket. This reproduces the iteration order of 2.4.1's chained hash table.
fn sort_by_bucket<W: Copy>(
    entries: &[(u32, W)],
    hash_size: u32,
    first_bucket: u32,
    bucket_len: &[u32],
) -> Vec<(u32, W)> {
    let mut start = Vec::with_capacity(bucket_len.len());
    let mut sum = 0usize;
    for &n in bucket_len {
        start.push(sum);
        sum += n as usize;
    }
    debug_assert_eq!(sum, entries.len());
    let Some(&first) = entries.first() else {
        return Vec::new();
    };
    let mut sorted = vec![first; entries.len()];
    for &e in entries {
        let b = (e.0 % hash_size - first_bucket) as usize;
        sorted[start[b]] = e;
        start[b] += 1;
    }
    sorted
}

/// Converts colors with their final weights to histogram items.
fn make_histogram(
    entries: impl Iterator<Item = (u32, f32)>,
    len: usize,
    lut: &ColorLut,
    cols: usize,
    rows: usize,
    ignorebits: u32,
) -> HistogramInternal {
    // Limit perceptual weight to 1/10th of the image surface area to prevent
    // a single color from dominating all others.
    let max_perceptual_weight = 0.1f32 * cols as f32 * rows as f32;
    let mut total_weight = 0f64;
    let mut items = Vec::with_capacity(len);
    for (color, weight) in entries {
        let [r, g, b, a] = color.to_le_bytes();
        let w = cmin(weight, max_perceptual_weight);
        total_weight += f64::from(w);
        items.push(HistItem {
            color: FPixel::from_rgba(lut, RGBA::new(r, g, b, a)),
            adjusted_weight: w,
            perceptual_weight: w,
            ..HistItem::default()
        });
    }
    HistogramInternal {
        items,
        total_perceptual_weight: total_weight,
        ignorebits,
    }
}

/// Pixels per band, the unit of parallel work. Fixed, so that results don't depend on the
/// number of threads.
const BAND_PIXELS: usize = 1 << 14;
/// Number of bucket ranges counted or merged independently.
const PARTITIONS: usize = 64;

/// Finds the bucket of 2.4.1's hash table (`color % hash_size`) and the partition of a color.
/// Partitions are contiguous, increasing ranges of buckets, see [`partition_buckets`].
struct Partitioner {
    hash_size: u32,
    /// `ceil(2^64 / hash_size)`, for [`Partitioner::bucket`].
    inverse: u64,
    partition_of_bucket: Vec<u8>,
}

impl Partitioner {
    fn new(hash_size: u32) -> Self {
        debug_assert!(hash_size > 1);
        let mut partition_of_bucket = vec![0; hash_size as usize];
        for p in 0..PARTITIONS {
            for b in partition_buckets(p, hash_size) {
                partition_of_bucket[b as usize] = p as u8;
            }
        }
        Self {
            hash_size,
            inverse: u64::MAX / u64::from(hash_size) + 1,
            partition_of_bucket,
        }
    }

    /// `color % hash_size` without a division: the fractional part of `color / hash_size`
    /// in 64-bit fixed point, multiplied by `hash_size`, is exact for all 32-bit operands
    /// (Lemire, Kaser, Kurz, "Faster remainder by direct computation", Software: Practice
    /// and Experience 49(6), 2019).
    #[inline(always)]
    fn bucket(&self, color: u32) -> u32 {
        let fraction = self.inverse.wrapping_mul(u64::from(color));
        ((u128::from(fraction) * u128::from(self.hash_size)) >> 64) as u32
    }

    #[inline(always)]
    fn partition(&self, color: u32) -> u8 {
        self.partition_of_bucket[self.bucket(color) as usize]
    }
}

/// Bucket range of 2.4.1's hash table covered by partition `p`: buckets `b` with
/// `b * PARTITIONS / hash_size == p`.
fn partition_buckets(p: usize, hash_size: u32) -> std::ops::Range<u32> {
    let hs = hash_size as usize;
    let start = |p: usize| (p * hs).div_ceil(PARTITIONS) as u32;
    start(p)..start(p + 1)
}

/// Splits pixels `pixels` (indices in row-major order) into fixed bands of `BAND_PIXELS`.
fn bands(pixels: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
    let end = pixels.end;
    pixels
        .step_by(BAND_PIXELS)
        .map(|i| i..cmin(end, i + BAND_PIXELS))
        .collect()
}

/// Calls `f(index of the first pixel, pixels)` for the parts of rows in `pixels`, in order.
fn for_each_row_part(image: &Image<'_>, pixels: std::ops::Range<usize>, mut f: impl FnMut(usize, &[RGBA])) {
    let cols = image.width;
    let mut temp_row = Vec::new();
    let mut start = pixels.start;
    while start < pixels.end {
        let row = start / cols;
        let row_start = row * cols;
        let end = cmin(pixels.end, row_start + cols);
        f(
            start,
            &image.row_rgba(row, &mut temp_row)[start - row_start..end - row_start],
        );
        start = end;
    }
}

/// `items.map(f)`, in parallel with the `threads` feature.
fn map_items<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(feature = "threads")]
    {
        use rayon::prelude::*;
        items.into_par_iter().map(f).collect()
    }
    #[cfg(not(feature = "threads"))]
    items.into_iter().map(f).collect()
}

/// Stable counting sort of `items` by their partitions `ids`.
fn group_by_partition<T: Copy>(items: &[T], ids: &[u8]) -> PartitionGroups<T> {
    debug_assert_eq!(items.len(), ids.len());
    let mut offsets = [0usize; PARTITIONS + 1];
    for &p in ids {
        offsets[usize::from(p) + 1] += 1;
    }
    for p in 0..PARTITIONS {
        offsets[p + 1] += offsets[p];
    }
    let mut next = offsets;
    let mut sorted = items.to_vec();
    for (&item, &p) in items.iter().zip(ids) {
        sorted[next[usize::from(p)]] = item;
        next[usize::from(p)] += 1;
    }
    PartitionGroups { items: sorted, offsets }
}

/// Items grouped by partition, in their original order within each partition.
struct PartitionGroups<T> {
    items: Vec<T>,
    /// `items[offsets[p]..offsets[p + 1]]` belong to partition `p`.
    offsets: [usize; PARTITIONS + 1],
}

impl<T> PartitionGroups<T> {
    fn get(&self, p: usize) -> &[T] {
        &self.items[self.offsets[p]..self.offsets[p + 1]]
    }
}

/// 2.4.1's per-bucket bookkeeping for its color limit check, for a range of buckets.
///
/// 2.4.1 kept the first two colors of each bucket inline and the rest in an overflow array,
/// and checked the color limit only when it grew that array (to 8 entries, then
/// `2 * capacity + 16`), so an image could have more colors than the limit and still pass.
struct BucketCounts {
    first: u32,
    /// Colors per bucket.
    len: Vec<u32>,
    /// Emulated capacity of each bucket's overflow array.
    capacity: Vec<u32>,
}

impl BucketCounts {
    fn new(buckets: std::ops::Range<u32>) -> Self {
        Self {
            first: buckets.start,
            len: vec![0; buckets.len()],
            capacity: vec![0; buckets.len()],
        }
    }

    /// Records a new color in `bucket`. Returns true if 2.4.1 grew the bucket's overflow
    /// array for it, which is where it checked the color limit.
    #[inline]
    fn insert(&mut self, bucket: u32) -> bool {
        let b = (bucket - self.first) as usize;
        let others = self.len[b].checked_sub(2);
        self.len[b] += 1;
        match others {
            Some(others) if others >= self.capacity[b] => {
                self.capacity[b] = if self.capacity[b] == 0 {
                    8
                } else {
                    self.capacity[b] * 2 + 16
                };
                true
            }
            _ => false,
        }
    }
}

/// The original's per-pixel importance boost `(0.5 + m / 255.0) as f32`, for each noise map value `m`.
fn importance_boosts() -> [f32; 256] {
    std::array::from_fn(|m| (0.5f64 + m as f64 / 255.0) as f32)
}

/// Images with at least this many pixels use [`compat_histogram_partitioned`].
#[cfg(feature = "threads")]
const PARALLEL_MIN_PIXELS: usize = 1 << 18;

/// Builds the histogram exactly like 2.4.1, or returns None when 2.4.1 would have
/// reported too many colors. Large images are counted in parallel with the `threads` feature.
fn compat_histogram(image: &Image<'_>, ignorebits: u32, maxcolors: u32, hash_size: u32) -> Option<HistogramInternal> {
    #[cfg(feature = "threads")]
    if image.width * image.height >= PARALLEL_MIN_PIXELS {
        return compat_histogram_partitioned(image, ignorebits, maxcolors, hash_size, CHUNK_PIXELS);
    }
    compat_histogram_sequential(image, ignorebits, maxcolors, hash_size)
}

/// [`compat_histogram`] in one pass over the image: weights are summed as `f32` in pixel
/// order, and the color limit is checked where 2.4.1 checked it (see [`BucketCounts`]).
fn compat_histogram_sequential(
    image: &Image<'_>,
    ignorebits: u32,
    maxcolors: u32,
    hash_size: u32,
) -> Option<HistogramInternal> {
    let (cols, rows) = (image.width, image.height);
    let posterize = Posterize::new(ignorebits);
    let boosts = importance_boosts();
    let mut table = ColorTable::<f32>::with_capacity(4096);
    let mut adder = RunAdder::new();
    let mut buckets = BucketCounts::new(0..hash_size);

    let mut temp_row = Vec::new();
    for row in 0..rows {
        let pixels = image.row_rgba(row, &mut temp_row);
        // noise map is used for importance: color accuracy in noisy areas is not very important
        let importance = image.noise.as_deref().map(|n| &n[row * cols..(row + 1) * cols]);
        for (col, &px) in pixels.iter().enumerate() {
            let boost = match importance {
                Some(map) => boosts[usize::from(map[col])],
                None => 1.0,
            };
            let color = posterize.color(px);
            // a new color starts at 0.0, and 0.0 + boost == boost exactly
            if adder.add(&mut table, color, boost)
                && buckets.insert(color % hash_size)
                && table.len > maxcolors as usize
            {
                return None;
            }
        }
    }
    adder.flush(&mut table);
    let len = table.len;
    let sorted = sort_by_bucket(&table.into_entries(), hash_size, 0, &buckets.len);
    Some(make_histogram(
        sorted.into_iter(),
        len,
        image.lut(),
        cols,
        rows,
        ignorebits,
    ))
}

/// Pixels per chunk of [`compat_histogram_partitioned`]. Bounds its temporary memory to
/// 12 bytes per pixel of a chunk (when no two neighbors have the same color).
#[cfg_attr(not(feature = "threads"), allow(dead_code))]
const CHUNK_PIXELS: usize = 1 << 21;

/// Pixels `start..start + len` of one color.
#[derive(Clone, Copy)]
struct Run {
    color: u32,
    start: u32,
    len: u32,
}

/// The colors of one partition (a range of buckets), counted like 2.4.1 counted them.
#[cfg_attr(not(feature = "threads"), allow(dead_code))]
struct CompatPartition {
    table: ColorTable<f32>,
    adder: RunAdder<f32>,
    buckets: BucketCounts,
    /// Index of the first pixel of each color, increasing.
    first_seen: Vec<u32>,
    /// Index of the pixel where 2.4.1 last checked the color limit in these buckets.
    last_check: Option<u32>,
    /// The limit was exceeded by the colors of this partition alone at a check.
    too_many: bool,
}

#[cfg_attr(not(feature = "threads"), allow(dead_code))]
impl CompatPartition {
    fn new(buckets: std::ops::Range<u32>) -> Self {
        Self {
            table: ColorTable::with_capacity(64),
            adder: RunAdder::new(),
            buckets: BucketCounts::new(buckets),
            first_seen: Vec::new(),
            last_check: None,
            too_many: false,
        }
    }

    /// Adds runs of pixels of this partition, in pixel order.
    fn add(&mut self, runs: &[Run], noise: Option<&[u8]>, boosts: &[f32; 256], hash_size: u32, maxcolors: u32) {
        if self.too_many {
            return;
        }
        for run in runs {
            let (color, index) = (run.color, run.start);
            let pixels = index as usize..(index + run.len) as usize;
            // the pixels of a run are added one by one, as in 2.4.1
            let new = match noise {
                Some(map) => {
                    let mut new = false;
                    for &m in &map[pixels] {
                        new |= self.adder.add(&mut self.table, color, boosts[usize::from(m)]);
                    }
                    new
                }
                None => {
                    let mut new = false;
                    for _ in pixels {
                        new |= self.adder.add(&mut self.table, color, 1.0);
                    }
                    new
                }
            };
            if new {
                self.first_seen.push(index);
                if self.buckets.insert(color % hash_size) {
                    self.last_check = Some(index);
                    // the image has at least as many colors as this partition
                    if self.table.len > maxcolors as usize {
                        self.too_many = true;
                        return;
                    }
                }
            }
        }
    }
}

/// [`compat_histogram`] in parallel, with exactly the same result.
///
/// Every color belongs to one partition (a range of 2.4.1's buckets). Bands of pixels are
/// split into per-partition lists of runs of equal colors in parallel, then each partition
/// takes its pixels in pixel order. So each color's `f32` weight gets the same additions in
/// the same order as in the sequential pass, and each bucket sees its colors in the same
/// order.
///
/// 2.4.1 failed at the first limit check (see [`BucketCounts`]) made while it had more than
/// `maxcolors` colors. Colors are counted in pixel order, so that is any check at or after
/// the pixel where the `maxcolors + 1`-th color first appears: the partitions record where
/// their colors first appear and where their checks happen, and the global condition is
/// tested after each chunk of rows (of about `chunk_pixels` pixels).
#[cfg_attr(not(feature = "threads"), allow(dead_code))]
fn compat_histogram_partitioned(
    image: &Image<'_>,
    ignorebits: u32,
    maxcolors: u32,
    hash_size: u32,
    chunk_pixels: usize,
) -> Option<HistogramInternal> {
    let (cols, rows) = (image.width, image.height);
    let posterize = Posterize::new(ignorebits);
    let boosts = importance_boosts();
    let noise = image.noise.as_deref();
    let partitioner = Partitioner::new(hash_size);
    let mut partitions: Vec<_> = (0..PARTITIONS)
        .map(|p| CompatPartition::new(partition_buckets(p, hash_size)))
        .collect();
    // first pixel at which the image has more than maxcolors colors
    let mut over_limit_at = None;

    let chunk_rows = (chunk_pixels / cols).max(1);
    for first_row in (0..rows).step_by(chunk_rows) {
        let chunk = first_row * cols..cmin(rows, first_row + chunk_rows) * cols;
        let groups = map_items(bands(chunk), |band| {
            let mut runs: Vec<Run> = Vec::new();
            let mut ids = Vec::new();
            for_each_row_part(image, band, |first, part| {
                for (i, &px) in (first as u32..).zip(part) {
                    let color = posterize.color(px);
                    match runs.last_mut() {
                        Some(run) if run.color == color => run.len += 1,
                        _ => {
                            runs.push(Run {
                                color,
                                start: i,
                                len: 1,
                            });
                            ids.push(partitioner.partition(color));
                        }
                    }
                }
            });
            group_by_partition(&runs, &ids)
        });
        let parts = std::mem::take(&mut partitions).into_iter().enumerate().collect();
        partitions = map_items(parts, |(p, mut part)| {
            for g in &groups {
                part.add(g.get(p), noise, &boosts, hash_size, maxcolors);
            }
            part
        });

        if partitions.iter().any(|p| p.too_many) {
            return None;
        }
        let colors: usize = partitions.iter().map(|p| p.table.len).sum();
        if colors > maxcolors as usize {
            // fixed once known: later colors appear later
            let limit = *over_limit_at.get_or_insert_with(|| {
                let mut first_seen: Vec<u32> = partitions.iter().flat_map(|p| p.first_seen.iter().copied()).collect();
                *first_seen.select_nth_unstable(maxcolors as usize).1
            });
            if partitions.iter().any(|p| p.last_check.is_some_and(|i| i >= limit)) {
                return None;
            }
        }
    }

    let colors = partitions.iter().map(|p| p.table.len).sum();
    let parts = map_items(partitions, |mut part| {
        part.adder.flush(&mut part.table);
        let first = part.buckets.first;
        sort_by_bucket(&part.table.into_entries(), hash_size, first, &part.buckets.len)
    });
    Some(make_histogram(
        parts.into_iter().flatten(),
        colors,
        image.lut(),
        cols,
        rows,
        ignorebits,
    ))
}

/// Counts the colors of the pixels in `band`, with weights in units of 1/510: the importance
/// boost `0.5 + m/255` of 2.4.1 is exactly `(255 + 2m)/510`, so integer sums are exact (and
/// fit `u32`, at most 765 per pixel). Returns the colors grouped by partition, or None if
/// the band alone has more than `maxcolors` colors.
fn count_band(
    image: &Image<'_>,
    band: std::ops::Range<usize>,
    posterize: Posterize,
    maxcolors: usize,
    partitioner: &Partitioner,
) -> Option<PartitionGroups<(u32, u32)>> {
    const _: () = assert!(BAND_PIXELS * 765 <= u32::MAX as usize);
    let mut table = ColorTable::<u32>::with_capacity(1024);
    let mut adder = RunAdder::new();
    let noise = image.noise.as_deref();
    for_each_row_part(image, band, |first, pixels| {
        // noise map is used for importance: color accuracy in noisy areas is not very important
        match noise {
            Some(noise) => {
                for (&px, &m) in pixels.iter().zip(&noise[first..first + pixels.len()]) {
                    adder.add(&mut table, posterize.color(px), 255 + 2 * u32::from(m));
                }
            }
            None => {
                for &px in pixels {
                    adder.add(&mut table, posterize.color(px), 510);
                }
            }
        }
    });
    if table.len > maxcolors {
        return None;
    }
    adder.flush(&mut table);
    let entries = table.into_entries();
    let ids: Vec<u8> = entries.iter().map(|&(color, _)| partitioner.partition(color)).collect();
    Some(group_by_partition(&entries, &ids))
}

/// Merges partition `p` of all bands in band order, then sorts it by bucket.
/// Returns None if the partition alone has more than `maxcolors` colors.
fn merge_partition(
    bands: &[PartitionGroups<(u32, u32)>],
    p: usize,
    maxcolors: usize,
    hash_size: u32,
) -> Option<Vec<(u32, u64)>> {
    let len = bands.iter().map(|b| b.get(p).len()).max().unwrap_or(0);
    let mut table = ColorTable::<u64>::with_capacity(len);
    for band in bands {
        for &(color, w) in band.get(p) {
            table.add(color, u64::from(w));
        }
        if table.len > maxcolors {
            return None;
        }
    }
    // Merging in band order keeps first-seen order, and integer sums are exact, so this
    // equals one sequential pass over the image.
    let buckets = partition_buckets(p, hash_size);
    let mut bucket_len = vec![0u32; buckets.len()];
    let table = table.into_entries();
    for &(color, _) in &table {
        bucket_len[(color % hash_size - buckets.start) as usize] += 1;
    }
    Some(sort_by_bucket(&table, hash_size, buckets.start, &bucket_len))
}

/// Builds the histogram with exact integer weights, in parallel over fixed bands of pixels,
/// or returns None if the image has more than `maxcolors` colors.
///
/// Unlike 2.4.1's lazy check, the limit is a plain limit on the number of colors, so near
/// the limit the posterization level can differ from [`Profile::Compat`]. Weights are not
/// rounded while summing, so a color's weight can also differ in the last bits, and is not
/// stuck at 2^24 pixels like an `f32` sum.
fn improved_histogram(image: &Image<'_>, ignorebits: u32, maxcolors: u32, hash_size: u32) -> Option<HistogramInternal> {
    let (cols, rows) = (image.width, image.height);
    let maxcolors = maxcolors as usize;
    let posterize = Posterize::new(ignorebits);

    let partitioner = Partitioner::new(hash_size);
    let bands = map_items(bands(0..rows * cols), |band| {
        count_band(image, band, posterize, maxcolors, &partitioner)
    });
    let bands: Vec<_> = bands.into_iter().collect::<Option<_>>()?;
    let partitions = map_items((0..PARTITIONS).collect(), |p| {
        merge_partition(&bands, p, maxcolors, hash_size)
    });
    drop(bands);
    let partitions: Vec<_> = partitions.into_iter().collect::<Option<_>>()?;

    let colors: usize = partitions.iter().map(Vec::len).sum();
    if colors > maxcolors {
        return None;
    }
    let entries = partitions
        .into_iter()
        .flatten()
        .map(|(color, w)| (color, (w as f64 / 510.0) as f32));
    Some(make_histogram(entries, colors, image.lut(), cols, rows, ignorebits))
}

/// Fixed colors are always included in the palette, so it would be wasteful to duplicate them in the palette from the histogram.
fn remove_fixed_colors_from_histogram(hist: &mut HistogramInternal, fixed_colors: &[FPixel], target_mse: f32) {
    if fixed_colors.is_empty() {
        return;
    }
    let max_difference = cmax(f64::from(target_mse) / 2.0, 2.0 / 256.0 / 256.0) as f32;
    let mut j = 0;
    while j < hist.items.len() {
        if fixed_colors
            .iter()
            .any(|f| hist.items[j].color.diff(f) < max_difference)
        {
            // overwritten with the last entry, which is checked next
            hist.items.swap_remove(j);
        } else {
            j += 1;
        }
    }
}

/// Histogram of how many times each color is present in the image, weighted by the importance map.
pub(crate) fn get_histogram(image: &mut Image<'_>, attr: &Attributes) -> Result<HistogramInternal, Error> {
    let mut ignorebits = u32::from(cmax(attr.min_posterization_output, attr.min_posterization_input));
    let surface = (image.width * image.height) as u64;

    if image.noise.is_none() && attr.use_contrast_maps {
        image.contrast_maps();
    }

    // Attempt to make a histogram of the colors, unclustered. If at first we don't succeed,
    // increase ignorebits to increase color coherence and try again.
    let mut hist = loop {
        let hash_size = hash_size(attr.max_histogram_entries, surface, ignorebits);
        let hist = match attr.profile {
            Profile::Compat => compat_histogram(image, ignorebits, attr.max_histogram_entries, hash_size),
            Profile::Improved => improved_histogram(image, ignorebits, attr.max_histogram_entries, hash_size),
        };
        if let Some(hist) = hist {
            break hist;
        }
        if ignorebits >= 7 {
            return Err(Error::OutOfMemory);
        }
        ignorebits += 1;
        attr.verbose_print(&format!(
            "  too many colors! Scaling colors to improve clustering... {ignorebits}"
        ));
    };

    image.noise = None;

    attr.verbose_print(&format!("  made histogram...{} colors found", hist.items.len()));
    remove_fixed_colors_from_histogram(&mut hist, &image.fixed_colors, attr.target_mse as f32);
    Ok(hist)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2.4.1's chained hash table (the previous implementation), kept as the reference.
    struct ReferenceHash {
        buckets: Vec<(Vec<(u32, f32)>, u32)>,
        hash_size: u32,
        maxcolors: u32,
        colors: u32,
        posterize: Posterize,
    }

    impl ReferenceHash {
        fn new(maxcolors: u32, surface: u64, ignorebits: u32) -> Self {
            let hash_size = hash_size(maxcolors, surface, ignorebits);
            Self {
                buckets: vec![(Vec::new(), 0); hash_size as usize],
                hash_size,
                maxcolors,
                colors: 0,
                posterize: Posterize::new(ignorebits),
            }
        }

        fn add_row(&mut self, pixels: &[RGBA], importance_map: Option<&[u8]>) -> bool {
            for (col, &px) in pixels.iter().enumerate() {
                let boost = match importance_map {
                    Some(map) => (0.5f64 + f64::from(map[col]) / 255.0) as f32,
                    None => 1.0,
                };
                let color = self.posterize.color(px);
                let (items, capacity) = &mut self.buckets[(color % self.hash_size) as usize];
                if let Some(entry) = items.iter_mut().find(|e| e.0 == color) {
                    entry.1 += boost;
                    continue;
                }
                if items.len() >= 2 {
                    let others = (items.len() - 2) as u32;
                    self.colors += 1;
                    if others >= *capacity {
                        if self.colors > self.maxcolors {
                            return false;
                        }
                        *capacity = if *capacity == 0 { 8 } else { *capacity * 2 + 16 };
                    }
                } else {
                    self.colors += 1;
                }
                items.push((color, boost));
            }
            true
        }

        /// Colors and weights in iteration order, or None if there are too many colors.
        fn build(image: &Image<'_>, ignorebits: u32, maxcolors: u32) -> Option<Vec<(u32, f32)>> {
            let (cols, rows) = (image.width, image.height);
            let mut table = Self::new(maxcolors, (cols * rows) as u64, ignorebits);
            let mut temp = Vec::new();
            for row in 0..rows {
                let pixels = image.row_rgba(row, &mut temp);
                let importance = image.noise.as_deref().map(|n| &n[row * cols..(row + 1) * cols]);
                if !table.add_row(pixels, importance) {
                    return None;
                }
            }
            Some(table.buckets.into_iter().flat_map(|b| b.0).collect())
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u32
        }
    }

    /// Image with `levels` values per channel (so a controllable number of colors), some
    /// transparent pixels with dirty RGB, and runs of repeated pixels.
    fn test_image(width: usize, height: usize, levels: u32, seed: u64) -> Vec<RGBA> {
        let mut rng = Rng(seed);
        let mut px = RGBA::new(0, 0, 0, 255);
        (0..width * height)
            .map(|_| {
                if rng.next() % 3 != 0 {
                    let ch = |v: u32| (v % levels * 255 / (levels - 1).max(1)) as u8;
                    let a = if rng.next() % 9 == 0 { ch(rng.next()) } else { 255 };
                    px = RGBA::new(ch(rng.next()), ch(rng.next()), ch(rng.next()), a);
                }
                px
            })
            .collect()
    }

    fn with_noise<'a>(attr: &Attributes, pixels: Vec<RGBA>, width: usize, height: usize, seed: u64) -> Image<'a> {
        let mut image = attr.new_image(pixels, width, height, 0.0).unwrap();
        if seed % 2 == 1 {
            let mut rng = Rng(seed);
            image.noise = Some((0..width * height).map(|_| rng.next() as u8).collect());
        }
        image
    }

    fn colors_and_weights(hist: &HistogramInternal) -> Vec<(FPixel, u32)> {
        hist.items
            .iter()
            .map(|i| (i.color, i.perceptual_weight.to_bits()))
            .collect()
    }

    /// Histogram items as 2.4.1 builds them from the reference table.
    fn reference_histogram(image: &Image<'_>, ignorebits: u32, maxcolors: u32) -> Option<HistogramInternal> {
        let entries = ReferenceHash::build(image, ignorebits, maxcolors)?;
        let len = entries.len();
        Some(make_histogram(
            entries.into_iter(),
            len,
            image.lut(),
            image.width,
            image.height,
            ignorebits,
        ))
    }

    const CASES: &[(usize, usize, u32)] = &[
        (1, 1, 2),
        (3, 2, 256),
        (17, 13, 4),
        (64, 64, 6),
        (100, 70, 256),
        (300, 250, 12),
        (400, 300, 256),
        (2000, 70, 33),
    ];

    #[test]
    fn compat_matches_2_4_1_hash_table() {
        let attr = crate::new();
        for (n, &(w, h, levels)) in CASES.iter().enumerate() {
            for seed in [n as u64 * 2, n as u64 * 2 + 1] {
                let image = with_noise(&attr, test_image(w, h, levels, seed), w, h, seed);
                let distinct = ReferenceHash::build(&image, 0, u32::MAX).unwrap().len() as u32;
                // see compat_keeps_lazy_limit_check for more limits close to the color count
                let limits = [1, distinct / 2, distinct - 1, u32::MAX];
                for ignorebits in [0, 1, 7] {
                    for &maxcolors in &limits {
                        let hs = hash_size(maxcolors, (w * h) as u64, ignorebits);
                        let expected = reference_histogram(&image, ignorebits, maxcolors);
                        let expected = expected.map(|e| (colors_and_weights(&e), e.total_perceptual_weight.to_bits()));
                        let sequential = compat_histogram_sequential(&image, ignorebits, maxcolors, hs);
                        // small chunks, so the limit is also checked between chunks
                        for actual in [
                            sequential,
                            compat_histogram_partitioned(&image, ignorebits, maxcolors, hs, 3000),
                        ] {
                            let actual = actual.map(|a| (colors_and_weights(&a), a.total_perceptual_weight.to_bits()));
                            assert!(expected == actual, "{w}x{h} {maxcolors} {ignorebits}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compat_keeps_lazy_limit_check() {
        // 2.4.1 accepts more colors than the limit when no bucket's overflow array grows
        // after the limit is reached; the emulated check must do the same.
        let attr = crate::new();
        let mut accepted_over_limit = false;
        for seed in 0..6 {
            let (w, h) = (90, 80);
            let image = with_noise(&attr, test_image(w, h, 7, seed), w, h, seed);
            let distinct = ReferenceHash::build(&image, 0, u32::MAX).unwrap().len() as u32;
            for maxcolors in (distinct.saturating_sub(200)..distinct).step_by(3) {
                let hs = hash_size(maxcolors, (w * h) as u64, 0);
                let expected = ReferenceHash::build(&image, 0, maxcolors).is_some();
                assert_eq!(
                    expected,
                    compat_histogram_sequential(&image, 0, maxcolors, hs).is_some()
                );
                for chunk in [1000, 1 << 20] {
                    let partitioned = compat_histogram_partitioned(&image, 0, maxcolors, hs, chunk);
                    assert_eq!(expected, partitioned.is_some());
                }
                accepted_over_limit |= expected;
            }
        }
        assert!(accepted_over_limit);
    }

    #[test]
    fn improved_has_exact_weights_in_2_4_1_order() {
        let attr = crate::new();
        for (n, &(w, h, levels)) in CASES.iter().enumerate() {
            for seed in [n as u64 * 2, n as u64 * 2 + 1] {
                let image = with_noise(&attr, test_image(w, h, levels, seed), w, h, seed);
                for ignorebits in [0, 2] {
                    let reference = ReferenceHash::build(&image, ignorebits, u32::MAX).unwrap();
                    let distinct = reference.len() as u32;
                    // exact sums: (255 + 2m) per pixel in units of 1/510, or 510 without a noise map
                    let posterize = Posterize::new(ignorebits);
                    let mut exact = std::collections::HashMap::new();
                    let mut temp = Vec::new();
                    for row in 0..h {
                        for (col, &px) in image.row_rgba(row, &mut temp).iter().enumerate() {
                            let units = image
                                .noise
                                .as_deref()
                                .map_or(510, |n| 255 + 2 * u64::from(n[row * w + col]));
                            *exact.entry(posterize.color(px)).or_insert(0u64) += units;
                        }
                    }
                    for maxcolors in [distinct - 1, distinct, u32::MAX] {
                        let hs = hash_size(maxcolors, (w * h) as u64, ignorebits);
                        let hist = improved_histogram(&image, ignorebits, maxcolors, hs);
                        // a plain limit on the number of colors
                        assert_eq!(hist.is_some(), distinct <= maxcolors);
                        let Some(hist) = hist else { continue };
                        // same colors, in the same order as 2.4.1
                        let reference = reference_histogram(&image, ignorebits, u32::MAX).unwrap();
                        assert_eq!(hist.items.len(), reference.items.len());
                        let max_weight = 0.1f32 * w as f32 * h as f32;
                        for ((color, _), (item, ref_item)) in ReferenceHash::build(&image, ignorebits, u32::MAX)
                            .unwrap()
                            .into_iter()
                            .zip(hist.items.iter().zip(&reference.items))
                        {
                            assert_eq!(item.color, ref_item.color);
                            let w = cmin((exact[&color] as f64 / 510.0) as f32, max_weight);
                            assert_eq!(item.perceptual_weight.to_bits(), w.to_bits());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn partitioner_matches_modulo() {
        for hash_size in [2, 3, 6673, 12011, 24019, 65521, u32::MAX] {
            let partitioner = Partitioner::new(hash_size.min(1 << 20));
            let hash_size = partitioner.hash_size;
            let mut rng = Rng(u64::from(hash_size));
            let edge = [0, 1, hash_size - 1, hash_size, hash_size + 1, u32::MAX - 1, u32::MAX];
            for color in edge.into_iter().chain((0..100_000).map(|_| rng.next())) {
                let bucket = color % hash_size;
                assert_eq!(partitioner.bucket(color), bucket);
                let p = usize::from(partitioner.partition(color));
                assert_eq!(p, bucket as usize * PARTITIONS / hash_size as usize);
                assert!(partition_buckets(p, hash_size).contains(&bucket));
            }
        }
    }

    fn histogram_hash(hist: &HistogramInternal) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let words = hist.items.iter().flat_map(|i| {
            let c = i.color;
            [c.a, c.r, c.g, c.b, i.perceptual_weight].map(f32::to_bits)
        });
        for w in words.map(u64::from).chain([hist.total_perceptual_weight.to_bits()]) {
            hash = (hash ^ w).wrapping_mul(0x100_0000_01b3);
        }
        hash
    }

    #[test]
    fn improved_is_the_same_with_and_without_threads() {
        // the same value in both builds (`--no-default-features` too)
        let mut attr = crate::new();
        attr.set_profile(crate::Profile::Improved);
        let (w, h) = (300, 250);
        let image = with_noise(&attr, test_image(w, h, 50, 3), w, h, 3);
        let hs = hash_size(u32::MAX, (w * h) as u64, 0);
        let hist = improved_histogram(&image, 0, u32::MAX, hs).unwrap();
        assert_eq!(histogram_hash(&hist), 3_642_948_049_464_613_470);
    }

    #[cfg(feature = "threads")]
    #[test]
    fn improved_is_the_same_for_any_thread_count() {
        let attr = crate::new();
        let (w, h) = (700, 500);
        let image = with_noise(&attr, test_image(w, h, 40, 5), w, h, 5);
        let run = |threads| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            pool.install(|| {
                let hs = hash_size(u32::MAX, (w * h) as u64, 0);
                let hist = improved_histogram(&image, 0, u32::MAX, hs).unwrap();
                (colors_and_weights(&hist), hist.total_perceptual_weight.to_bits())
            })
        };
        let one = run(1);
        assert_eq!(one, run(3));
        assert_eq!(one, run(8));
    }
}
