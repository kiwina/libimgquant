//! Voronoi iteration: each palette color moves to the weighted average of the colors
//! that map to it.

use crate::attr::Profile;
use crate::hist::{HistItem, HistogramInternal};
use crate::nearest::Nearest;
use crate::pal::{FPixel, PalF};

#[derive(Clone, Copy, Default)]
struct ViterState {
    a: f64,
    r: f64,
    g: f64,
    b: f64,
    total: f64,
}

pub(crate) struct Kmeans {
    averages: Vec<ViterState>,
}

impl Kmeans {
    pub fn new(colors: usize) -> Self {
        Self {
            averages: vec![ViterState::default(); colors],
        }
    }

    #[inline(always)]
    pub fn update_color(&mut self, color: FPixel, value: f32, index: usize) {
        let s = &mut self.averages[index];
        s.a += f64::from(color.a * value);
        s.r += f64::from(color.r * value);
        s.g += f64::from(color.g * value);
        s.b += f64::from(color.b * value);
        s.total += f64::from(value);
    }

    /// Adds partial sums from another part of the image, in a fixed order.
    #[cfg(feature = "threads")]
    pub fn merge(&mut self, other: &Self) {
        for (s, o) in self.averages.iter_mut().zip(&other.averages) {
            s.a += o.a;
            s.r += o.r;
            s.g += o.g;
            s.b += o.b;
            s.total += o.total;
        }
    }

    pub fn finalize(&self, map: &mut PalF) {
        for (i, (item, s)) in map.items.iter_mut().zip(&self.averages).enumerate() {
            let mut total = s.total;
            if total != 0.0 && !item.fixed {
                item.color = FPixel {
                    a: (s.a / total) as f32,
                    r: (s.r / total) as f32,
                    g: (s.g / total) as f32,
                    b: (s.b / total) as f32,
                };
            } else {
                total = i as f64 / 1024.0;
            }
            item.popularity = total as f32;
        }
    }
}

/// Histogram entries per parallel work unit. Fixed so results don't depend on thread count.
#[cfg(feature = "threads")]
const CHUNK: usize = 4096;

/// Remaps the histogram to the palette, moves palette colors to their Voronoi centroids,
/// and returns the weighted mean error. With `adjust_weights` set, poorly matched
/// histogram entries get more weight for the next median cut.
pub(crate) fn viter_do_iteration(
    hist: &mut HistogramInternal,
    map: &mut PalF,
    min_opaque_val: f32,
    adjust_weights: bool,
    fast_palette: bool,
    profile: Profile,
) -> f64 {
    let n = Nearest::new(map, fast_palette, profile);
    let colors = map.len();

    let process = |items: &mut [HistItem]| {
        let mut kmeans = Kmeans::new(colors);
        let mut total_diff = 0f64;
        for item in items {
            let (matched, diff) = n.search(item.color, usize::from(item.likely_colormap_index), min_opaque_val);
            item.likely_colormap_index = matched as u8;
            total_diff += f64::from(diff * item.perceptual_weight);
            kmeans.update_color(item.color, item.perceptual_weight, matched);
            if adjust_weights {
                item.adjusted_weight = (item.perceptual_weight + item.adjusted_weight) * (1.0 + diff).sqrt();
            }
        }
        (kmeans, total_diff)
    };

    #[cfg(feature = "threads")]
    let (kmeans, total_diff) = if hist.items.len() > CHUNK {
        use rayon::prelude::*;
        let parts: Vec<_> = hist.items.par_chunks_mut(CHUNK).map(process).collect();
        let mut parts = parts.into_iter();
        let (mut kmeans, mut total_diff) = parts.next().expect("non-empty");
        for (k, d) in parts {
            kmeans.merge(&k);
            total_diff += d;
        }
        (kmeans, total_diff)
    } else {
        process(&mut hist.items)
    };
    #[cfg(not(feature = "threads"))]
    let (kmeans, total_diff) = process(&mut hist.items);

    drop(n);
    kmeans.finalize(map);
    total_diff / hist.total_perceptual_weight
}

#[inline(always)]
fn dist2(x: &[f64; 6], y: &[f64; 6]) -> f64 {
    x.iter().zip(y).map(|(a, b)| (a - b) * (a - b)).sum()
}

/// Same coordinates as [`FPixel::embed`], for a color given as `[a, r, g, b]`.
#[inline(always)]
fn embed4(c: [f64; 4]) -> [f64; 6] {
    let [a, r, g, b] = c;
    [r, g, b, r - a, g - a, b - a]
}

/// Weighted Hartigan refinement of a palette over the histogram (Hartigan & Wong, "A
/// K-Means Clustering Algorithm", Applied Statistics 28(1), 1979; Telgarsky & Vattani,
/// "Hartigan's Method: k-means Clustering without Voronoi", AISTATS 2010).
///
/// Lloyd iterations stop when every color is nearest to its own centroid. Hartigan's rule
/// also moves a color when that lowers the total error after both centroids are updated:
/// moving weight `w` from cluster `a` to `j` changes the error by
/// `W_j/(W_j+w)·d(x,c_j) − W_a/(W_a−w)·d(x,c_a)`. Every Hartigan-stable partition is
/// Lloyd-stable, but not the reverse, and the difference matters most for heavy
/// histogram entries. Early passes inflate the cost of staying by up to 1.5× to escape
/// shallow minima; the last pass uses the exact rule.
///
/// The color difference is a squared Euclidean distance (see [`FPixel::embed`]), so
/// candidates are pruned exactly with the triangle inequality, using centroid distances
/// from the start of the pass plus a bound on how far centroids have moved since.
/// Clusters lighter than `tau` cost less to join than the bound assumes, so they are
/// always checked. Runs sequentially in histogram order, so it is deterministic.
///
/// Items in fixed clusters never move, and fixed centroids never change. Returns the
/// number of moves; palette colors and popularity are updated from the new clusters.
pub(crate) fn hartigan_refine(hist: &HistogramInternal, map: &mut PalF, passes: u32) -> usize {
    hartigan_refine_impl::<true>(hist, map, passes, 8)
}

/// `PRUNE = false` walks every row to the end, for testing that pruning is exact. Below
/// `direct_factor` histogram entries per palette color, every cluster is checked directly.
fn hartigan_refine_impl<const PRUNE: bool>(
    hist: &HistogramInternal,
    map: &mut PalF,
    passes: u32,
    direct_factor: usize,
) -> usize {
    let k = map.len();
    if k < 2 || hist.items.is_empty() || passes == 0 {
        return 0;
    }
    let fixed: Vec<bool> = map.items.iter().map(|p| p.fixed).collect();
    let mut sum = vec![[0f64; 4]; k];
    let mut weight = vec![0f64; k];
    let mut assign = Vec::with_capacity(hist.items.len());
    for item in &hist.items {
        let a = usize::from(item.likely_colormap_index);
        let a = if a < k { a } else { 0 };
        let w = f64::from(item.perceptual_weight);
        let c = item.color;
        for (s, v) in sum[a].iter_mut().zip([c.a, c.r, c.g, c.b]) {
            *s += w * f64::from(v);
        }
        weight[a] += w;
        assign.push(a);
    }

    let centroid = |sum: &[f64; 4], weight: f64| sum.map(|s| s / weight);
    // empty clusters that aren't fixed are left alone; they aren't move targets
    let target = |j: usize, weight: &[f64]| fixed[j] || weight[j] > 0.0;
    let mut centers: Vec<[f64; 6]> = (0..k)
        .map(|j| {
            if fixed[j] || weight[j] == 0.0 {
                map.items[j].color.embed()
            } else {
                embed4(centroid(&sum[j], weight[j]))
            }
        })
        .collect();
    let nonempty = weight.iter().filter(|&&w| w > 0.0).count().max(1);
    let tau = 0.05 * weight.iter().sum::<f64>() / nonempty as f64;

    let direct = hist.items.len() < direct_factor.saturating_mul(k);
    let mut total_moves = 0;
    for pass in 0..passes {
        let factor = if pass + 1 < passes {
            1.5 - 0.5 * f64::from(pass) / f64::from(passes)
        } else {
            1.0
        };
        let built = centers.clone();
        // other clusters ordered by distance from each centroid
        // For small histograms, checking every cluster costs less than building the rows.
        let rows: Vec<Vec<(f64, usize)>> = (0..if direct { 0 } else { k })
            .map(|a| {
                let mut row: Vec<(f64, usize)> = (0..k)
                    .filter(|&j| j != a && target(j, &weight))
                    .map(|j| (dist2(&built[a], &built[j]).sqrt(), j))
                    .collect();
                row.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                row
            })
            .collect();
        let mut drift = vec![0f64; k];
        let mut max_drift = 0f64;
        let mut is_small: Vec<bool> = (0..k)
            .map(|j| !fixed[j] && weight[j] > 0.0 && weight[j] < tau)
            .collect();
        let mut small: Vec<usize> = (0..k).filter(|&j| is_small[j]).collect();

        let mut moves = 0;
        for (i, item) in hist.items.iter().enumerate() {
            let a = assign[i];
            let w = f64::from(item.perceptual_weight);
            let remaining = weight[a] - w;
            // the last item of a cluster stays, so that no palette entry is emptied
            if fixed[a] || remaining <= weight[a] * 1e-9 {
                continue;
            }
            let x = item.color.embed();
            let qa = dist2(&x, &centers[a]);
            let mut best = factor * weight[a] / remaining * qa;
            let mut best_j = a;
            let consider = |j: usize, best: &mut f64, best_j: &mut usize| {
                let scale = if fixed[j] { 1.0 } else { weight[j] / (weight[j] + w) };
                let cost = scale * dist2(&x, &centers[j]);
                if cost < *best {
                    *best = cost;
                    *best_j = j;
                }
            };

            // |x - c_j| >= |c_a' - c_j'| - |x - c_a| - drift(a) - drift(j), where ' is the start of the pass
            let da = qa.sqrt();
            let scale_bound = tau / (tau + w);
            if direct {
                for j in (0..k).filter(|&j| j != a && target(j, &weight)) {
                    consider(j, &mut best, &mut best_j);
                }
            } else {
                for &(dcc, j) in &rows[a] {
                    let lb = dcc - da - drift[a] - max_drift - 1e-9;
                    if PRUNE && lb > 0.0 && scale_bound * lb * lb >= best {
                        break;
                    }
                    if !is_small[j] {
                        consider(j, &mut best, &mut best_j);
                    }
                }
                for &j in &small {
                    if j != a {
                        consider(j, &mut best, &mut best_j);
                    }
                }
            }

            if best_j != a {
                let j = best_j;
                let c = item.color;
                for (idx, v) in [c.a, c.r, c.g, c.b].into_iter().enumerate() {
                    sum[a][idx] -= w * f64::from(v);
                    sum[j][idx] += w * f64::from(v);
                }
                weight[a] = remaining;
                weight[j] += w;
                for t in [a, j] {
                    if !fixed[t] {
                        centers[t] = embed4(centroid(&sum[t], weight[t]));
                        let d = dist2(&centers[t], &built[t]).sqrt() * (1.0 + 1e-6) + 1e-12;
                        drift[t] = d;
                        max_drift = max_drift.max(d);
                    }
                }
                if !is_small[a] && weight[a] < tau {
                    is_small[a] = true;
                    small.push(a);
                }
                assign[i] = j;
                moves += 1;
            }
        }
        total_moves += moves;
        // inflated passes move at least as much as exact ones, so nothing more would move
        if moves == 0 {
            break;
        }
    }

    for (j, item) in map.items.iter_mut().enumerate() {
        if !fixed[j] && weight[j] > 0.0 {
            let [a, r, g, b] = centroid(&sum[j], weight[j]);
            item.color = FPixel {
                a: a as f32,
                r: r as f32,
                g: g as f32,
                b: b as f32,
            };
        }
        item.popularity = weight[j] as f32;
    }
    total_moves
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pal::PalItem;

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }

        fn pixel(&mut self) -> FPixel {
            let a = if self.next() < 0.3 { self.next() } else { 1.0 };
            FPixel {
                a,
                r: self.next() * a,
                g: self.next() * a,
                b: self.next() * a,
            }
        }
    }

    fn random_case(seed: u64, items: usize, colors: usize) -> (HistogramInternal, PalF) {
        let mut rng = Lcg(seed);
        let palette: Vec<PalItem> = (0..colors)
            .map(|i| PalItem {
                color: rng.pixel(),
                popularity: 0.0,
                fixed: i % 7 == 3,
            })
            .collect();
        let hist_items: Vec<HistItem> = (0..items)
            .map(|_| {
                let color = rng.pixel();
                let nearest = (0..colors)
                    .min_by(|&x, &y| color.diff(&palette[x].color).total_cmp(&color.diff(&palette[y].color)))
                    .unwrap();
                let w = 0.5 + rng.next() * 20.0;
                HistItem {
                    color,
                    adjusted_weight: w,
                    perceptual_weight: w,
                    likely_colormap_index: nearest as u8,
                    ..HistItem::default()
                }
            })
            .collect();
        let total = hist_items.iter().map(|h| f64::from(h.perceptual_weight)).sum();
        (
            HistogramInternal {
                items: hist_items,
                total_perceptual_weight: total,
                ignorebits: 0,
            },
            PalF {
                items: palette,
                subset: None,
            },
        )
    }

    /// Weighted squared error with every color assigned to its nearest palette entry.
    fn sse(hist: &HistogramInternal, map: &PalF) -> f64 {
        hist.items
            .iter()
            .map(|h| {
                let e = h.color.embed();
                let best = map
                    .items
                    .iter()
                    .map(|p| dist2(&e, &p.color.embed()))
                    .fold(f64::MAX, f64::min);
                f64::from(h.perceptual_weight) * best
            })
            .sum()
    }

    #[test]
    fn pruning_is_exact() {
        for seed in 0..40 {
            let (hist, map) = random_case(seed, 300 + seed as usize * 37, 2 + seed as usize * 6);
            let (mut pruned, mut full) = (map.clone(), map);
            let moves = hartigan_refine_impl::<true>(&hist, &mut pruned, 6, 0);
            assert_eq!(
                moves,
                hartigan_refine_impl::<false>(&hist, &mut full, 6, 0),
                "seed {seed}"
            );
            for (p, f) in pruned.items.iter().zip(&full.items) {
                assert_eq!(p.color, f.color, "seed {seed}");
            }
            // checking every cluster directly finds the same moves (ties have probability zero here)
            let (_, map) = random_case(seed, 300 + seed as usize * 37, 2 + seed as usize * 6);
            let mut direct_map = map;
            assert_eq!(
                moves,
                hartigan_refine_impl::<true>(&hist, &mut direct_map, 6, usize::MAX),
                "seed {seed}"
            );
            for (p, d) in pruned.items.iter().zip(&direct_map.items) {
                assert_eq!(p.color, d.color, "seed {seed}");
            }
        }
    }

    #[test]
    fn exact_passes_do_not_increase_error() {
        for seed in 100..130 {
            let (hist, mut map) = random_case(seed, 500, 16);
            let before = sse(&hist, &map);
            hartigan_refine(&hist, &mut map, 1);
            let after = sse(&hist, &map);
            assert!(after <= before * (1.0 + 1e-9), "seed {seed}: {before} -> {after}");
            assert!(map.items.iter().all(|p| p.color.a.is_finite()));
        }
    }
}
