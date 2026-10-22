//! Median cut of the `Improved` profile: variance-based splits under the exact color metric.
//!
//! Three changes to Heckbert's median cut, taken from the variance-based quantizers that beat
//! it both alone and as a k-means initializer (Celebi, "Improving the performance of k-means
//! for color quantization", Image and Vision Computing 29(4), 2011):
//!
//! 1. The box to split is the one with the largest error, as in Wan, Prusinkiewicz & Wong,
//!    "Variance-based color image quantization for frame buffer display", Color Research &
//!    Application 15(1), 1990, and Wu, "Efficient Statistical Computations for Optimal Color
//!    Quantization", Graphics Gems II, 1991. The `max_error` escalation of the 2.4.1 code is
//!    kept, so boxes holding colors far from their palette color still go first.
//! 2. The cut is perpendicular to the box's principal axis (Orchard & Bouman, "Color
//!    Quantization of Images", IEEE Trans. Signal Processing 39(12), 1991; Wu, ACM TOG 11(4), 1992).
//! 3. The cut is placed where the error of the two halves is least (Wan et al.; Wu).
//!
//! The error is not the plain sum of squared errors (SSE) of those papers, but an estimate of
//! `Σ w·|x − x̄|^1.25` (see [`box_cost`]): minimizing SSE alone measurably lowered the MSE
//! but raised SSIM and OKLab ΔE error, because it gives too few colors to popular smooth
//! areas. With the lower exponent PSNR, SSIM, ΔE and its 99th percentile all improve.
//!
//! All statistics are exact for the black/white blending metric, which is a quadratic form
//! (see [`super::moments`]), and come from additive moments (Wu 1991), so they cost O(1) per
//! box instead of a pass over its colors. The threshold search is O(n): keys are bucketed into
//! [`BINS`] bins of moments, whose prefix sums score every cut, and the children's moments,
//! mean colors and split directions follow from the bins. A split of a large box therefore
//! takes two passes over it: one to fill the bins, and an in-place partition that also
//! measures the children's maximum error and key ranges. Boxes with at most [`BINS`] items
//! are cut exactly instead, by sorting their keys; they fit in cache.
//!
//! The palette color of a box is its weighted mean (the least-error color under the metric),
//! with the `min_opaque_val` rounding of `average_pixels`; 2.4.1's weighting of colors far
//! from the parent's color made no measurable difference here. Popularity is the box's
//! perceptual weight. The feedback-loop steps (`adjust_histogram`, the representative subset
//! and the target-error check) are shared with the 2.4.1 code.

use super::moments::{Linear, Moments, key};
use super::{HistBox, adjust_histogram, total_box_error_below_target};
use crate::hist::{HistItem, HistogramInternal};
use crate::pal::{FPixel, PalF, PalItem};

/// Bins of the threshold search. Boxes up to this size are searched exactly.
const BINS: usize = 512;

struct WBox {
    ind: usize,
    colors: usize,
    moments: Moments,
    color: FPixel,
    /// [`box_cost`] of the box.
    cost: f64,
    max_error: f64,
    /// Calculated lazily.
    total_error: Option<f64>,
    /// Projection for the next split of a box that will be split by bins (more than [`BINS`]
    /// items), and the range of its keys, which the bins need. They're measured in the pass
    /// that creates the box. Smaller boxes get their direction when they are split.
    dir: Option<[f64; 4]>,
    lo: f64,
    hi: f64,
}

impl HistBox for WBox {
    fn span(&self) -> std::ops::Range<usize> {
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

impl WBox {
    /// A box whose statistics are known, except the ones that need its final color and
    /// direction: those are gathered with [`observe`](Self::observe).
    fn from_moments(ind: usize, moments: Moments, min_opaque_val: f32) -> Self {
        Self {
            ind,
            colors: moments.count,
            color: moments.color(min_opaque_val),
            cost: box_cost(&moments.linear()),
            dir: (moments.count > BINS).then(|| moments.split_direction()),
            moments,
            max_error: 0.0,
            total_error: None,
            lo: f64::INFINITY,
            hi: f64::NEG_INFINITY,
        }
    }

    fn from_items(achv: &[HistItem], ind: usize, colors: usize, min_opaque_val: f32) -> Self {
        let items = &achv[ind..ind + colors];
        let mut b = Self::from_moments(ind, Moments::of(items), min_opaque_val);
        for item in items {
            b.observe(item);
        }
        b
    }

    #[inline(always)]
    fn observe(&mut self, item: &HistItem) {
        if let Some(dir) = &self.dir {
            let k = key(dir, item.color);
            if k < self.lo {
                self.lo = k;
            }
            if k > self.hi {
                self.hi = k;
            }
        }
        let e = f64::from(self.color.diff(&item.color));
        if e > self.max_error {
            self.max_error = e;
        }
    }
}

/// Which side of a cut an item goes to.
enum Cut {
    /// Bins `0..=last` of an equal-width binning of `[lo, hi]` go left.
    Bin { lo: f64, scale: f64, last: usize },
    /// Keys up to and including this one go left.
    Key(f64),
}

impl Cut {
    #[inline(always)]
    fn is_left(&self, k: f64) -> bool {
        match *self {
            Self::Bin { lo, scale, last } => bin_index(k, lo, scale) <= last,
            Self::Key(t) => k <= t,
        }
    }
}

#[inline(always)]
fn bin_index(k: f64, lo: f64, scale: f64) -> usize {
    // k >= lo, so this is not negative; `as` saturates
    (((k - lo) * scale) as usize).min(BINS - 1)
}

/// Error of a box, for choosing boxes and cuts: an estimate of `Σ w·|x − x̄|^p` with p = 1.25.
///
/// For a box of weight W whose colors have RMS distance `σ = √(SSE/W)` from their mean,
/// `Σ w·|x − x̄|^p ≈ W·σ^p = W^(1−p/2)·SSE^(p/2)`, exact up to a factor that depends only on
/// the shape of the distribution. p = 2 is the SSE criterion of Wan et al. and Wu. In
/// high-resolution quantization theory the optimal density of palette colors is proportional
/// to `density^(d/(d+p))` (Gersho, "Asymptotically optimal block quantization", IEEE Trans.
/// Information Theory 25(4), 1979), and splitting the box of largest `W·σ^p` approaches it:
/// lower p puts more colors into popular regions such as smooth gradients, higher p into
/// outliers. PSNR is a p = 2 measure, mean ΔE a p = 1 measure; 2.4.1's priority `W·Σw·d²`
/// concentrates colors like p = 1. On the evaluation corpus p = 2 lowered the MSE but raised
/// SSIM and ΔE error, p = 1 the reverse, and p = 1.2-1.5 improved all of them at speeds 1-7.
#[inline(always)]
fn box_cost(l: &Linear) -> f64 {
    let sse = l.sse().max(0.0);
    // W^(3/8)·SSE^(5/8) = ((W·SSE)³·SSE²)^(1/8), with square roots only
    let t = l.w * sse;
    (t * t * t * sse * sse).sqrt().sqrt().sqrt()
}

/// Picks the cut after `bins[i]` that minimizes the cost of both sides. Lowest index wins ties.
fn best_cut(bins: &[Moments], nonempty: &mut Vec<(usize, Linear)>) -> Option<usize> {
    nonempty.clear();
    let mut total = Linear::default();
    for (i, b) in bins.iter().enumerate() {
        if b.count > 0 {
            let l = b.linear();
            total.add(&l);
            nonempty.push((i, l));
        }
    }
    // a cut after the last non-empty bin leaves the right side empty
    let (_, candidates) = nonempty.split_last()?;
    let mut left = Linear::default();
    let mut best = None;
    let mut best_cost = f64::INFINITY;
    for &(i, ref l) in candidates {
        left.add(l);
        let cost = box_cost(&left) + box_cost(&total.minus(&left));
        if cost < best_cost {
            best_cost = cost;
            best = Some(i);
        }
    }
    best
}

/// Scratch space reused by all splits of one median cut.
#[derive(Default)]
struct Scratch {
    bins: Vec<Moments>,
    /// non-empty bins and their [`Linear`] moments
    nonempty: Vec<(usize, Linear)>,
    /// (key, index) of the items of a small box, sorted
    order: Vec<(f64, u32)>,
    /// moments of the items of a small box, in key order
    linear: Vec<Linear>,
}

/// Picks the cut of a box with more than [`BINS`] items. Returns the moments of both sides
/// too, summed directly from the bins (not total minus left), so small sides keep full precision.
fn binned_cut(b: &WBox, dir: &[f64; 4], items: &[HistItem], scratch: &mut Scratch) -> Option<(Cut, Moments, Moments)> {
    if b.hi <= b.lo {
        return None;
    }
    let (lo, scale) = (b.lo, BINS as f64 / (b.hi - b.lo));
    if !scale.is_finite() {
        return None;
    }
    let bins = &mut scratch.bins;
    bins.clear();
    bins.resize(BINS, Moments::default());
    for item in items {
        bins[bin_index(key(dir, item.color), lo, scale)].add(item);
    }
    let best = best_cut(bins, &mut scratch.nonempty)?;
    let cut = Cut::Bin { lo, scale, last: best };
    Some((cut, Moments::sum(&bins[..=best]), Moments::sum(&bins[best + 1..])))
}

/// Picks the cut of a small box exactly: every boundary between distinct keys is scored.
/// Only (key, index) pairs are sorted, which is much cheaper than moving the items.
fn exact_cut(dir: &[f64; 4], items: &[HistItem], scratch: &mut Scratch) -> Option<Cut> {
    let order = &mut scratch.order;
    order.clear();
    order.extend(
        items
            .iter()
            .enumerate()
            .map(|(i, item)| (key(dir, item.color), i as u32)),
    );
    order.sort_unstable_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    let linear = &mut scratch.linear;
    linear.clear();
    let mut total = Linear::default();
    for &(_, i) in order.iter() {
        let l = Linear::of_item(&items[i as usize]);
        total.add(&l);
        linear.push(l);
    }
    let mut left = Linear::default();
    let mut best = None;
    let mut best_cost = f64::INFINITY;
    for (pair, l) in order.windows(2).zip(linear.iter()) {
        left.add(l);
        if pair[0].0 == pair[1].0 {
            continue; // equal keys stay together
        }
        let cost = box_cost(&left) + box_cost(&total.minus(&left));
        if cost < best_cost {
            best_cost = cost;
            best = Some(pair[0].0);
        }
    }
    best.map(Cut::Key)
}

/// Splits a box in two along its principal axis, at the threshold of least [`box_cost`].
fn split_box(b: &WBox, achv: &mut [HistItem], scratch: &mut Scratch, min_opaque_val: f32) -> (WBox, WBox) {
    let items = &mut achv[b.ind..b.ind + b.colors];
    let dir = b.dir.unwrap_or_else(|| b.moments.split_direction());
    let found = if items.len() > BINS {
        binned_cut(b, &dir, items, scratch).map(|(cut, l, r)| (cut, Some((l, r))))
    } else {
        exact_cut(&dir, items, scratch).map(|cut| (cut, None))
    };
    let Some((cut, moments)) = found else {
        // no spread along the axis (degenerate weights): split by position
        let half = b.colors / 2;
        return (
            WBox::from_items(achv, b.ind, half, min_opaque_val),
            WBox::from_items(achv, b.ind + half, b.colors - half, min_opaque_val),
        );
    };
    let is_left = |item: &HistItem| cut.is_left(key(&dir, item.color));

    if let Some((left, right)) = moments {
        // The children are known from the bins, so one in-place partition also measures their
        // maximum errors and key ranges. Every item is classified and observed exactly once.
        let mut lower = WBox::from_moments(b.ind, left, min_opaque_val);
        let mut upper = WBox::from_moments(b.ind + left.count, right, min_opaque_val);
        let lower_end = partition(items, is_left, |item| lower.observe(item), |item| upper.observe(item));
        // keys are recomputed by the same function, so the sides match the bins exactly
        debug_assert_eq!(lower_end, lower.colors);
        (lower, upper)
    } else {
        // small box: gather the moments while partitioning, then measure the children (in cache)
        let (mut left, mut right) = (Moments::default(), Moments::default());
        let half = partition(items, is_left, |item| left.add(item), |item| right.add(item));
        let mut lower = WBox::from_moments(b.ind, left, min_opaque_val);
        let mut upper = WBox::from_moments(b.ind + half, right, min_opaque_val);
        for item in &items[..half] {
            lower.observe(item);
        }
        for item in &items[half..] {
            upper.observe(item);
        }
        (lower, upper)
    }
}

/// In-place partition: items for which `is_left` holds go first. Each item is classified once
/// and passed once to `on_left` or `on_right`. Returns the number of left items.
#[inline(always)]
fn partition(
    items: &mut [HistItem],
    is_left: impl Fn(&HistItem) -> bool,
    mut on_left: impl FnMut(&HistItem),
    mut on_right: impl FnMut(&HistItem),
) -> usize {
    let (mut i, mut j) = (0, items.len());
    loop {
        while i < j && is_left(&items[i]) {
            on_left(&items[i]);
            i += 1;
        }
        while i < j && !is_left(&items[j - 1]) {
            on_right(&items[j - 1]);
            j -= 1;
        }
        if i >= j {
            return i;
        }
        // items[i] belongs right, items[j - 1] left
        on_right(&items[i]);
        on_left(&items[j - 1]);
        items.swap(i, j - 1);
        i += 1;
        j -= 1;
    }
}

/// The box with the largest cost, escalated for boxes containing colors that exceed the quality limit.
fn best_splittable_box(boxes: &[WBox], max_mse: f64) -> Option<usize> {
    let mut bi = None;
    let mut maxsum = 0f64;
    for (i, b) in boxes.iter().enumerate() {
        if b.colors < 2 {
            continue;
        }
        let mut thissum = b.cost;
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

fn colormap_from_boxes(boxes: &[WBox]) -> Vec<PalItem> {
    boxes
        .iter()
        .map(|b| PalItem {
            color: b.color,
            popularity: b.moments.pw as f32,
            fixed: false,
        })
        .collect()
}

/// Splits the histogram into at most `newcolors` boxes and returns their mean colors.
pub(super) fn mediancut(
    hist: &mut HistogramInternal,
    min_opaque_val: f32,
    newcolors: usize,
    target_mse: f64,
    max_mse: f64,
) -> PalF {
    let total_perceptual_weight = hist.total_perceptual_weight;
    let achv = &mut hist.items[..];
    let mut scratch = Scratch::default();

    let mut boxes = Vec::with_capacity(newcolors);
    boxes.push(WBox::from_items(achv, 0, achv.len(), min_opaque_val));

    // remember smaller palette for fast searching
    let mut representative_subset = None;
    let subset_size = (newcolors as f32).powf(0.7).ceil() as usize;

    while boxes.len() < newcolors {
        if boxes.len() == subset_size {
            representative_subset = Some(colormap_from_boxes(&boxes));
        }

        // as in 2.4.1: first split boxes that exceed the quality limit, then raise the limit
        let current_max_mse = max_mse + (boxes.len() as f64 / newcolors as f64) * 16.0 * max_mse;
        let Some(bi) = best_splittable_box(&boxes, current_max_mse) else {
            break; // ran out of colors!
        };

        let (lower, upper) = split_box(&boxes[bi], achv, &mut scratch, min_opaque_val);
        boxes[bi] = lower;
        boxes.push(upper);

        if total_box_error_below_target(target_mse, &mut boxes, total_perceptual_weight, achv) {
            break;
        }
    }

    let map = PalF {
        items: colormap_from_boxes(&boxes),
        subset: representative_subset,
    };
    adjust_histogram(achv, &map, &boxes);
    map
}

#[cfg(test)]
mod tests {
    use super::super::moments::tests::random_items;
    use super::*;

    fn check_box(achv: &[HistItem], b: &WBox, min_opaque_val: f32) {
        let items = &achv[b.span()];
        let fresh = WBox::from_items(achv, b.ind, b.colors, min_opaque_val);
        assert_eq!(b.colors, fresh.moments.count);
        assert_eq!(b.moments.opaque, fresh.moments.opaque);
        let rel = |x: f64, y: f64| (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1e-12);
        assert!(rel(b.moments.w, fresh.moments.w));
        assert!(
            (b.cost - fresh.cost).abs() <= 1e-7 * fresh.cost.max(1e-9),
            "{} {}",
            b.cost,
            fresh.cost
        );
        for (x, y) in [
            (b.color.a, fresh.color.a),
            (b.color.r, fresh.color.r),
            (b.color.b, fresh.color.b),
        ] {
            assert!((x - y).abs() < 1e-6);
        }
        // max error and key range are exact for the box's own color and direction
        let max_error = items
            .iter()
            .map(|i| f64::from(b.color.diff(&i.color)))
            .fold(0.0, f64::max);
        assert_eq!(b.max_error, max_error);
        assert_eq!(b.dir.is_some(), b.colors > BINS);
        if let Some(dir) = &b.dir {
            let keys: Vec<f64> = items.iter().map(|i| key(dir, i.color)).collect();
            assert_eq!(b.lo, keys.iter().copied().fold(f64::INFINITY, f64::min));
            assert_eq!(b.hi, keys.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        }
    }

    fn cost_of(items: &[HistItem]) -> f64 {
        box_cost(&Moments::of(items).linear())
    }

    /// Brute force: the least cost(left) + cost(right) over all cuts between distinct keys.
    fn brute_force_cost(items: &[HistItem], dir: &[f64; 4]) -> f64 {
        let mut sorted = items.to_vec();
        sorted.sort_by(|x, y| key(dir, x.color).total_cmp(&key(dir, y.color)));
        let mut best = f64::INFINITY;
        for i in 1..sorted.len() {
            if key(dir, sorted[i - 1].color) == key(dir, sorted[i].color) {
                continue;
            }
            let cost = cost_of(&sorted[..i]) + cost_of(&sorted[i..]);
            best = best.min(cost);
        }
        best
    }

    #[test]
    fn exact_split_is_optimal_and_partitions_correctly() {
        for seed in 0..60 {
            let n = 2 + seed as usize * 8; // up to BINS: exact path
            let mut achv = random_items(seed, n + 6);
            let (ind, colors) = (3, n);
            let b = WBox::from_items(&achv, ind, colors, 1.0);
            let dir = b.moments.split_direction();
            let expected = brute_force_cost(&achv[ind..ind + colors], &dir);
            let before = achv.clone();
            let mut scratch = Scratch::default();
            let (l, r) = split_box(&b, &mut achv, &mut scratch, 1.0);
            assert_eq!((l.ind, l.colors + r.colors, r.ind), (ind, colors, ind + l.colors));
            assert!(l.colors >= 1 && r.colors >= 1);
            let got = cost_of(&achv[l.span()]) + cost_of(&achv[r.span()]);
            assert!(
                (got - expected).abs() <= 1e-7 * expected.max(1e-9),
                "seed {seed}: {got} vs {expected}"
            );
            // left keys all below right keys
            let lmax = achv[l.span()]
                .iter()
                .map(|i| key(&dir, i.color))
                .fold(f64::MIN, f64::max);
            let rmin = achv[r.span()]
                .iter()
                .map(|i| key(&dir, i.color))
                .fold(f64::MAX, f64::min);
            assert!(lmax < rmin);
            check_box(&achv, &l, 1.0);
            check_box(&achv, &r, 1.0);
            // items outside the box are untouched
            assert_eq!(format!("{:?}", &achv[..ind]), format!("{:?}", &before[..ind]));
            assert_eq!(
                format!("{:?}", &achv[ind + colors..]),
                format!("{:?}", &before[ind + colors..])
            );
        }
    }

    #[test]
    fn binned_split_is_near_optimal_and_partitions_correctly() {
        for seed in 0..6 {
            let n = 600 + seed as usize * 700;
            let mut achv = random_items(1000 + seed, n);
            let b = WBox::from_items(&achv, 0, n, 0.9);
            let dir = b.moments.split_direction();
            let expected = brute_force_cost(&achv, &dir);
            let parent_cost = b.cost;
            let mut scratch = Scratch::default();
            let (l, r) = split_box(&b, &mut achv, &mut scratch, 0.9);
            assert_eq!(l.colors + r.colors, n);
            let got = cost_of(&achv[l.span()]) + cost_of(&achv[r.span()]);
            // within a bin of the optimum: much closer to it than to not splitting at all
            assert!(got >= expected * (1.0 - 1e-9));
            assert!(got - expected <= 0.01 * (parent_cost - expected), "{got} vs {expected}");
            let lmax = achv[l.span()]
                .iter()
                .map(|i| key(&dir, i.color))
                .fold(f64::MIN, f64::max);
            let rmin = achv[r.span()]
                .iter()
                .map(|i| key(&dir, i.color))
                .fold(f64::MAX, f64::min);
            assert!(lmax <= rmin);
            check_box(&achv, &l, 0.9);
            check_box(&achv, &r, 0.9);
        }
    }

    #[test]
    fn identical_keys_fall_back_to_a_positional_split() {
        let mut achv = random_items(5, 1000);
        for item in &mut achv {
            item.color = FPixel::splat(0.5);
        }
        let b = WBox::from_items(&achv, 0, 1000, 1.0);
        let (l, r) = split_box(&b, &mut achv, &mut Scratch::default(), 1.0);
        assert_eq!((l.colors, r.colors), (500, 500));
        assert!(l.cost < 1e-6, "{}", l.cost); // zero up to cancellation in the moments
    }

    fn hist(items: Vec<HistItem>) -> HistogramInternal {
        let total_perceptual_weight = items.iter().map(|i| f64::from(i.perceptual_weight)).sum();
        HistogramInternal {
            items,
            total_perceptual_weight,
            ignorebits: 0,
        }
    }

    #[test]
    fn mediancut_covers_the_histogram_deterministically() {
        for (seed, n, colors) in [(1, 3000, 256), (2, 300, 16), (3, 50, 64), (4, 1, 8)] {
            let mut h1 = hist(random_items(seed, n));
            let mut h2 = hist(random_items(seed, n));
            let p1 = mediancut(&mut h1, 1.0, colors, 0.0, 1e20);
            let p2 = mediancut(&mut h2, 1.0, colors, 0.0, 1e20);
            assert_eq!(p1.len(), colors.min(n));
            assert_eq!(format!("{:?}", p1.items), format!("{:?}", p2.items));
            assert_eq!(format!("{:?}", h1.items), format!("{:?}", h2.items));
            // every item is assigned to its box's palette entry, and popularity is the perceptual weight
            let mut pop = vec![0f64; p1.len()];
            for item in &h1.items {
                let i = usize::from(item.likely_colormap_index);
                assert!(i < p1.len());
                pop[i] += f64::from(item.perceptual_weight);
            }
            for (p, item) in pop.iter().zip(&p1.items) {
                assert!((p - f64::from(item.popularity)).abs() <= 1e-3 * p.max(1.0));
            }
            let subset_size = (colors as f32).powf(0.7).ceil() as usize;
            assert_eq!(p1.subset.is_some(), p1.len() > subset_size);
        }
    }

    #[test]
    fn mediancut_stops_at_target_mse() {
        // 8 tight clusters
        let mut items = random_items(9, 4000);
        let centers = random_items(10, 8);
        for (i, item) in items.iter_mut().enumerate() {
            let c = centers[i % 8].color;
            let n = item.color;
            item.color = FPixel {
                a: c.a,
                r: c.r + n.r * 0.01,
                g: c.g + n.g * 0.01,
                b: c.b + n.b * 0.01,
            };
        }
        let mut h = hist(items);
        let map = mediancut(&mut h, 1.0, 256, 1e-4, 1e20);
        assert!(map.len() >= 8 && map.len() < 64, "{}", map.len());
        let total: f64 = h
            .items
            .iter()
            .map(|i| {
                f64::from(map.items[usize::from(i.likely_colormap_index)].color.diff(&i.color) * i.perceptual_weight)
            })
            .sum();
        assert!(total <= 1e-4 * h.total_perceptual_weight * 1.0001);
    }

    #[test]
    fn opaque_rounding_applies_to_box_colors() {
        // an opaque and a slightly transparent color in one box
        let mut items = random_items(11, 2);
        items[0].color = FPixel {
            a: 1.0,
            r: 0.5,
            g: 0.2,
            b: 0.3,
        };
        items[1].color = FPixel {
            a: 0.9,
            r: 0.45,
            g: 0.18,
            b: 0.27,
        };
        let map = mediancut(&mut hist(items.clone()), 0.9, 1, 0.0, 1e20);
        let c = map.items[0].color;
        assert_eq!(c.a, 1.0);
        assert!((c.r - 0.5).abs() < 1e-6 && (c.g - 0.2).abs() < 1e-6 && (c.b - 0.3).abs() < 1e-6);
        let map = mediancut(&mut hist(items), 1.0, 1, 0.0, 1e20);
        assert!(map.items[0].color.a < 1.0);
    }
}
