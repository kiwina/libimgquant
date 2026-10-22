//! Median cut palette generator.
//!
//! Based on Paul Heckbert's paper "Color Image Quantization for Frame Buffer
//! Display", SIGGRAPH 1982 Proceedings, page 297.

mod improved;
mod moments;

use std::cmp::Ordering;
use std::ops::Range;

use crate::attr::Profile;
use crate::hist::{HistItem, HistogramInternal};
use crate::pal::{FPixel, PalF, PalItem, cmax, cmin};

/// What the steps shared by both profiles need to know about a box.
trait HistBox {
    /// Indices of the box's items in the histogram.
    fn span(&self) -> Range<usize>;
    /// The box's palette color.
    fn color(&self) -> FPixel;
    fn total_error(&self) -> Option<f64>;
    fn set_total_error(&mut self, e: f64);
}

struct MBox {
    color: FPixel,
    variance: FPixel,
    sum: f64,
    /// Calculated lazily.
    total_error: Option<f64>,
    max_error: f64,
    ind: usize,
    colors: usize,
}

impl MBox {
    fn new(achv: &[HistItem], ind: usize, colors: usize, sum: f64, min_opaque_val: f32, center: FPixel) -> Self {
        let items = &achv[ind..ind + colors];
        let color = average_pixels(items, min_opaque_val, center);
        Self {
            color,
            variance: box_variance(items, color),
            sum,
            total_error: None,
            max_error: box_max_error(items, color),
            ind,
            colors,
        }
    }

    fn items<'a>(&self, achv: &'a [HistItem]) -> &'a [HistItem] {
        &achv[self.ind..self.ind + self.colors]
    }
}

impl HistBox for MBox {
    fn span(&self) -> Range<usize> {
        self.ind..self.ind + self.colors
    }

    fn color(&self) -> FPixel {
        self.color
    }

    fn total_error(&self) -> Option<f64> {
        self.total_error
    }

    fn set_total_error(&mut self, e: f64) {
        self.total_error = Some(e);
    }
}

#[inline(always)]
fn variance_diff(val: f64, good_enough: f64) -> f64 {
    let val = val * val;
    if val < good_enough * good_enough {
        val * 0.25
    } else {
        val
    }
}

/// Weighted per-channel variance of the box. It's used to decide which channel to split by.
fn box_variance(items: &[HistItem], mean: FPixel) -> FPixel {
    let (mut va, mut vr, mut vg, mut vb) = (0f64, 0f64, 0f64, 0f64);
    for item in items {
        let px = item.color;
        let weight = f64::from(item.adjusted_weight);
        va += variance_diff(f64::from(mean.a - px.a), 2.0 / 256.0) * weight;
        vr += variance_diff(f64::from(mean.r - px.r), 1.0 / 256.0) * weight;
        vg += variance_diff(f64::from(mean.g - px.g), 1.0 / 256.0) * weight;
        vb += variance_diff(f64::from(mean.b - px.b), 1.0 / 256.0) * weight;
    }
    FPixel {
        a: (va * (4.0 / 16.0)) as f32,
        r: (vr * (7.0 / 16.0)) as f32,
        g: (vg * (9.0 / 16.0)) as f32,
        b: (vb * (5.0 / 16.0)) as f32,
    }
}

fn box_max_error(items: &[HistItem], mean: FPixel) -> f64 {
    let mut max_error = 0f64;
    for item in items {
        let diff = f64::from(mean.diff(&item.color));
        if diff > max_error {
            max_error = diff;
        }
    }
    max_error
}

#[inline(always)]
fn qsort_pivot(base: &[HistItem]) -> usize {
    let len = base.len();
    if len < 32 {
        return len / 2;
    }
    let (aidx, bidx, cidx) = (8, len / 2, len - 1);
    let (a, b, c) = (base[aidx].sort_value, base[bidx].sort_value, base[cidx].sort_value);
    if a < b {
        if b < c {
            bidx
        } else if a < c {
            cidx
        } else {
            aidx
        }
    } else if b > c {
        bidx
    } else if a < c {
        aidx
    } else {
        cidx
    }
}

/// Partitions in descending `sort_value` order, returning the final pivot position.
#[inline(always)]
fn qsort_partition(base: &mut [HistItem]) -> usize {
    let len = base.len();
    let (mut l, mut r) = (1, len);
    if len >= 8 {
        let pivot = qsort_pivot(base);
        base.swap(0, pivot);
    }
    let pivot_value = base[0].sort_value;
    while l < r {
        if base[l].sort_value >= pivot_value {
            l += 1;
        } else {
            loop {
                r -= 1;
                if !(l < r && base[r].sort_value <= pivot_value) {
                    break;
                }
            }
            base.swap(l, r);
        }
    }
    l -= 1;
    base.swap(0, l);
    l
}

/// Quick select: sorts only as much as needed to place the element at `sort_start`.
fn hist_item_sort_range(mut base: &mut [HistItem], mut sort_start: usize) {
    loop {
        let l = qsort_partition(base);
        let r = l + 1;
        if l > 0 && sort_start < l {
            base = &mut base[..l];
        } else if r < base.len() && sort_start > r {
            base = &mut base[r..];
            sort_start -= r;
        } else {
            break;
        }
    }
}

/// Sorts `achv[start..start+len]` so that the sum of color weights is lower than `halfvar` on one side.
/// Returns the absolute index of the edge item.
///
/// The original recursed into the left side of each partition. That recursion can be as
/// deep as the box is large (many items with equal sort keys), so it is kept on a heap
/// stack here, visiting partitions in exactly the same order.
///
/// A left-side range ending without a result looks one element past its end: that is the
/// pivot of the enclosing range. At the top level that would be outside the box (the
/// original read out of bounds), so `box_end` bounds it.
fn hist_item_sort_halfvar(
    achv: &mut [HistItem],
    mut start: usize,
    mut len: usize,
    lowervar: &mut f64,
    halfvar: f64,
    box_end: usize,
) -> Option<usize> {
    // ranges whose left side is being processed: (start, len, pivot)
    let mut enclosing: Vec<(usize, usize, usize)> = Vec::new();
    'partition: loop {
        let l = qsort_partition(&mut achv[start..start + len]);

        // check if sum of left side is smaller than half,
        // if it is, then it doesn't need to be sorted
        let mut t = 0;
        let mut tmpsum = *lowervar;
        while t <= l && tmpsum < halfvar {
            tmpsum += f64::from(achv[start + t].color_weight);
            t += 1;
        }

        if tmpsum < halfvar {
            *lowervar = tmpsum;
        } else if l > 0 {
            enclosing.push((start, len, l));
            len = l;
            continue 'partition;
        } else {
            // End of left descent. This will be executed in order from the first element.
            *lowervar += f64::from(achv[start].color_weight);
            if *lowervar > halfvar {
                return Some(start);
            }
        }

        // continue with the right side of this range, or finish it and resume the enclosing one
        let (mut s, mut n, mut pivot) = (start, len, l);
        loop {
            let r = pivot + 1;
            if n > r {
                start = s + r;
                len = n - r;
                continue 'partition;
            }
            let idx = s + r;
            if idx < box_end {
                *lowervar += f64::from(achv[idx].color_weight);
                if *lowervar > halfvar {
                    return Some(idx);
                }
            }
            (s, n, pivot) = enclosing.pop()?;
        }
    }
}

#[inline(always)]
fn color_weight(median: FPixel, h: &HistItem) -> f64 {
    let mut diff = median.diff(&h.color);
    // if color is "good enough", don't split further
    if diff < 2.0 / 256.0 / 256.0 {
        diff /= 2.0;
    }
    f64::from(diff).sqrt() * ((1.0 + f64::from(h.adjusted_weight)).sqrt() - 1.0)
}

/// Finds which channels need to be sorted first and preprocesses `achv` for fast sort.
fn prepare_sort(b: &MBox, achv: &mut [HistItem]) -> f64 {
    // Sort dimensions by their variance, and then sort colors first by dimension with highest variance.
    // Channel indices follow `FPixel::channel`: a=0, r=1, g=2, b=3.
    let mut channels = [
        (1usize, b.variance.r),
        (2, b.variance.g),
        (3, b.variance.b),
        (0, b.variance.a),
    ];
    channels.sort_by(|x, y| {
        if x.1 > y.1 {
            Ordering::Less
        } else if x.1 < y.1 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    });
    let (c0, c1, c2, c3) = (channels[0].0, channels[1].0, channels[2].0, channels[3].0);

    let items = &mut achv[b.ind..b.ind + b.colors];
    for item in items.iter_mut() {
        let px = item.color;
        // Only the first channel really matters. When trying median cut many times
        // with different histogram weights, sort randomness must not influence the outcome.
        let primary = (f64::from(px.channel(c0)) * 65535.0) as u32;
        let rest = ((f64::from(px.channel(c2)) + f64::from(px.channel(c1)) / 2.0 + f64::from(px.channel(c3)) / 4.0)
            * 65535.0) as u32;
        item.sort_value = (primary << 16) | rest;
    }

    let median = get_median(b, achv);

    // box will be split to make color_weight of each side even
    let mut totalvar = 0f64;
    for item in &mut achv[b.ind..b.ind + b.colors] {
        item.color_weight = color_weight(median, item) as f32;
        totalvar += f64::from(item.color_weight);
    }
    totalvar / 2.0
}

/// Finds median in unsorted set by sorting only the minimum required.
fn get_median(b: &MBox, achv: &mut [HistItem]) -> FPixel {
    let median_start = (b.colors - 1) / 2;
    hist_item_sort_range(&mut achv[b.ind..b.ind + b.colors], median_start);

    if b.colors & 1 == 1 {
        return achv[b.ind + median_start].color;
    }

    // technically the second color is not guaranteed to be sorted correctly
    // but most of the time it is good enough to be useful
    let start = b.ind + median_start;
    average_pixels(&achv[start..start + 2], 1.0, FPixel::splat(0.5))
}

/// Finds the best splittable box, if any box is splittable.
fn best_splittable_box(boxes: &[MBox], max_mse: f64) -> Option<usize> {
    let mut bi = None;
    let mut maxsum = 0f64;
    for (i, b) in boxes.iter().enumerate() {
        if b.colors < 2 {
            continue;
        }

        // looks only at max variance, because it's only going to split by it
        let cv = f64::from(cmax(b.variance.r, cmax(b.variance.g, b.variance.b)));
        let mut thissum = b.sum * cmax(f64::from(b.variance.a), cv);

        if b.max_error > max_mse {
            thissum = thissum * b.max_error / max_mse;
        }

        if thissum > maxsum {
            maxsum = thissum;
            bi = Some(i);
        }
    }
    bi
}

fn box_error(b: &impl HistBox, achv: &[HistItem]) -> f64 {
    let avg = b.color();
    let mut total_error = 0f64;
    for item in &achv[b.span()] {
        total_error += f64::from(avg.diff(&item.color) * item.perceptual_weight);
    }
    total_error
}

fn total_box_error_below_target(
    target_mse: f64,
    boxes: &mut [impl HistBox],
    total_perceptual_weight: f64,
    achv: &[HistItem],
) -> bool {
    let target_mse = target_mse * total_perceptual_weight;
    let mut total_error = 0f64;

    for b in boxes.iter() {
        if let Some(e) = b.total_error() {
            total_error += e;
        }
        if total_error > target_mse {
            return false;
        }
    }

    for b in boxes.iter_mut() {
        if b.total_error().is_none() {
            let e = box_error(b, achv);
            b.set_total_error(e);
            total_error += e;
        }
        if total_error > target_mse {
            return false;
        }
    }
    true
}

fn colormap_from_boxes(boxes: &[MBox], achv: &[HistItem]) -> Vec<PalItem> {
    // Representative color of each box is its (weighted) average, as in Heckbert's paper.
    boxes
        .iter()
        .map(|b| {
            let mut popularity = 0f32;
            for item in b.items(achv) {
                popularity += item.perceptual_weight;
            }
            PalItem {
                color: b.color,
                popularity,
                fixed: false,
            }
        })
        .collect()
}

/// Increases histogram popularity by difference from the final color (used as part of the feedback loop).
fn adjust_histogram(achv: &mut [HistItem], map: &PalF, boxes: &[impl HistBox]) {
    for (bi, b) in boxes.iter().enumerate() {
        let pal_color = map.items[bi].color;
        for item in &mut achv[b.span()] {
            item.adjusted_weight =
                (f64::from(item.adjusted_weight) * (1.0 + f64::from(pal_color.diff(&item.color)) / 4.0).sqrt()) as f32;
            item.likely_colormap_index = bi as u8;
        }
    }
}

fn average_pixels(items: &[HistItem], min_opaque_val: f32, center: FPixel) -> FPixel {
    let (mut r, mut g, mut b, mut a) = (0f64, 0f64, 0f64, 0f64);
    let mut new_a = 0f64;
    let mut sum = 0f64;
    let mut maxa = 0f32;

    // first find final opacity in order to blend colors at that opacity
    for item in items {
        let px = item.color;
        new_a += f64::from(px.a * item.adjusted_weight);
        sum += f64::from(item.adjusted_weight);
        // find if there are opaque colors, in case we're supposed to preserve opacity exactly (ie_bug)
        if px.a > maxa {
            maxa = px.a;
        }
    }

    if sum != 0.0 {
        new_a /= sum;
    }

    // if there was at least one completely opaque color, "round" final color to opaque
    if new_a >= f64::from(min_opaque_val) && f64::from(maxa) >= 255.0 / 256.0 {
        new_a = 1.0;
    }

    sum = 0.0;
    // reverse iteration for cache locality with previous loop
    for item in items.iter().rev() {
        let mut px = item.color;

        // give more weight to colors that are further away from average.
        // this is intended to prevent desaturation of images and fading of whites
        let mut weight = 1.0f64;
        let tmp = f64::from(center.r - px.r);
        weight += tmp * tmp;
        let tmp = f64::from(center.g - px.g);
        weight += tmp * tmp;
        let tmp = f64::from(center.b - px.b);
        weight += tmp * tmp;

        weight *= f64::from(item.adjusted_weight);
        sum += weight;

        if px.a != 0.0 {
            px.r /= px.a;
            px.g /= px.a;
            px.b /= px.a;
        }

        r += f64::from(px.r) * new_a * weight;
        g += f64::from(px.g) * new_a * weight;
        b += f64::from(px.b) * new_a * weight;
        a += new_a * weight;
    }

    if sum != 0.0 {
        a /= sum;
        r /= sum;
        g /= sum;
        b /= sum;
    }

    debug_assert!(!r.is_nan() && !g.is_nan() && !b.is_nan() && !a.is_nan());
    FPixel {
        a: a as f32,
        r: r as f32,
        g: g as f32,
        b: b as f32,
    }
}

/// Splits the histogram into at most `newcolors` boxes and returns their average colors.
///
/// `Profile::Improved` uses variance-based splits instead (see [`improved`]).
pub(crate) fn mediancut(
    hist: &mut HistogramInternal,
    min_opaque_val: f32,
    newcolors: usize,
    target_mse: f64,
    max_mse: f64,
    profile: Profile,
) -> PalF {
    match profile {
        Profile::Compat => {}
        Profile::Improved => return improved::mediancut(hist, min_opaque_val, newcolors, target_mse, max_mse),
    }
    let total_perceptual_weight = hist.total_perceptual_weight;
    let achv = &mut hist.items[..];

    let mut sum = 0f64;
    for item in achv.iter() {
        sum += f64::from(item.adjusted_weight);
    }
    let mut boxes = Vec::with_capacity(newcolors);
    boxes.push(MBox::new(achv, 0, achv.len(), sum, min_opaque_val, FPixel::splat(0.5)));

    // remember smaller palette for fast searching
    let mut representative_subset = None;
    let subset_size = (newcolors as f32).powf(0.7).ceil() as usize;

    // Main loop: split boxes until we have enough.
    while boxes.len() < newcolors {
        if boxes.len() == subset_size {
            representative_subset = Some(colormap_from_boxes(&boxes, achv));
        }

        // first splits boxes that exceed quality limit (to have colors for things like odd green pixel),
        // later raises the limit to allow large smooth areas/gradients get colors.
        let current_max_mse = max_mse + (boxes.len() as f64 / newcolors as f64) * 16.0 * max_mse;
        let Some(bi) = best_splittable_box(&boxes, current_max_mse) else {
            break; // ran out of colors!
        };

        let indx = boxes[bi].ind;
        let clrs = boxes[bi].colors;

        // Classic implementation tries to get even number of colors or pixels in each subdivision.
        // Here, instead of popularity, (sqrt(popularity)*variance) is balanced: boxes can be large
        // if they have similar colors, and boxes with high variance are more likely to be split.
        // Median used as expected value gives much better results than mean.
        let halfvar = prepare_sort(&boxes[bi], achv);
        let mut lowervar = 0f64;

        // sorts and sums lowervar at the same time; returns the item to break at, minus one
        let break_at = match hist_item_sort_halfvar(achv, indx, clrs, &mut lowervar, halfvar, indx + clrs) {
            Some(p) => cmin(clrs - 1, p - indx + 1),
            None => clrs - 1,
        };

        // Split the box.
        let sm = boxes[bi].sum;
        let mut lowersum = 0f64;
        for item in &achv[indx..indx + break_at] {
            lowersum += f64::from(item.adjusted_weight);
        }

        let previous_center = boxes[bi].color;
        boxes[bi] = MBox::new(achv, indx, break_at, lowersum, min_opaque_val, previous_center);
        boxes.push(MBox::new(
            achv,
            indx + break_at,
            clrs - break_at,
            sm - lowersum,
            min_opaque_val,
            previous_center,
        ));

        if total_box_error_below_target(target_mse, &mut boxes, total_perceptual_weight, achv) {
            break;
        }
    }

    let map = PalF {
        items: colormap_from_boxes(&boxes, achv),
        subset: representative_subset,
    };
    adjust_histogram(achv, &map, &boxes);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original recursive formulation, kept to check the iterative one against it.
    fn sort_halfvar_recursive(
        achv: &mut [HistItem],
        mut start: usize,
        mut len: usize,
        lowervar: &mut f64,
        halfvar: f64,
        box_end: usize,
    ) -> Option<usize> {
        loop {
            let l = qsort_partition(&mut achv[start..start + len]);
            let r = l + 1;
            let mut t = 0;
            let mut tmpsum = *lowervar;
            while t <= l && tmpsum < halfvar {
                tmpsum += f64::from(achv[start + t].color_weight);
                t += 1;
            }
            if tmpsum < halfvar {
                *lowervar = tmpsum;
            } else if l > 0 {
                if let Some(res) = sort_halfvar_recursive(achv, start, l, lowervar, halfvar, box_end) {
                    return Some(res);
                }
            } else {
                *lowervar += f64::from(achv[start].color_weight);
                if *lowervar > halfvar {
                    return Some(start);
                }
            }
            if len > r {
                start += r;
                len -= r;
            } else {
                let idx = start + r;
                if idx >= box_end {
                    return None;
                }
                *lowervar += f64::from(achv[idx].color_weight);
                return if *lowervar > halfvar { Some(idx) } else { None };
            }
        }
    }

    fn items(seed: u64, n: usize, distinct_keys: u32) -> Vec<HistItem> {
        let mut s = seed;
        let mut next = move || {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (s >> 33) as u32
        };
        (0..n)
            .map(|_| HistItem {
                sort_value: next() % distinct_keys,
                color_weight: (next() % 1000) as f32 / 997.0,
                ..HistItem::default()
            })
            .collect()
    }

    #[test]
    fn iterative_halfvar_matches_recursive() {
        for seed in 0..400 {
            let n = 2 + (seed as usize * 7) % 300;
            let distinct = [1, 2, 3, 17, 1 << 20][seed as usize % 5];
            let base = items(seed, n + 5, distinct);
            // a box in the middle of the histogram, and one at its end
            for (start, len) in [(3, n), (5, n)] {
                let total: f64 = base[start..start + len].iter().map(|i| f64::from(i.color_weight)).sum();
                for halfvar in [0.0, total / 2.0, total * 0.9, total * 2.0] {
                    let (mut a, mut b) = (base.clone(), base.clone());
                    let (mut va, mut vb) = (0.0, 0.0);
                    let ra = hist_item_sort_halfvar(&mut a, start, len, &mut va, halfvar, start + len);
                    let rb = sort_halfvar_recursive(&mut b, start, len, &mut vb, halfvar, start + len);
                    assert_eq!(ra, rb, "seed {seed} halfvar {halfvar}");
                    assert_eq!(va.to_bits(), vb.to_bits());
                    let order = |v: &[HistItem]| {
                        v.iter()
                            .map(|i| (i.sort_value, i.color_weight.to_bits()))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(order(&a), order(&b));
                }
            }
        }
    }

    #[test]
    fn halfvar_with_equal_keys_does_not_overflow_the_stack() {
        // every partition peels off one element; the recursive version needed ~n/2 frames
        let mut achv = vec![
            HistItem {
                sort_value: 7,
                color_weight: 1.0,
                ..HistItem::default()
            };
            20_000
        ];
        let len = achv.len();
        let mut lowervar = 0.0;
        let res = hist_item_sort_halfvar(&mut achv, 0, len, &mut lowervar, len as f64 / 2.0, len);
        assert_eq!(res, Some(len / 2));
    }
}
