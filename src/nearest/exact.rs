//! Exact nearest palette color (`Profile::Improved`).
//!
//! The color difference is a squared Euclidean distance in a linear embedding of the colors
//! (see [`embed`]), so its square root is a metric and the triangle inequality holds. The
//! search returns the true minimum of the difference plus the 2.4.1 penalty for making holes
//! in IE, unlike the 2.4.1 vantage-point heads, which miss the nearest color in about 1.5% of
//! the searches that the guess does not answer.
//!
//! **Result.** The index `j` minimizing `dist(embed(px), embed(palette[j])) + penalty(j)`,
//! where `penalty(j) = 1/1024` if `px.a > min_opaque_val` and `palette[j].a < 1`, else 0. Ties
//! go to the lowest index. The returned difference is that minimum, rounded to `f32`. The
//! result depends neither on the guess nor on the order of searches, so it is the same for any
//! thread count and with or without the `threads` feature. [`ExactNearest::brute_force`] is
//! the definition; the faster paths are tested against it.
//!
//! **Algorithm.**
//! 1. The guess shortcut of 2.4.1: if the guess is closer than half the distance to its nearest
//!    other palette color, no other color can be closer (with a margin for rounding).
//! 2. Otherwise, Orchard's search ("A fast nearest-neighbor search algorithm", ICASSP 1991),
//!    in its switching form: each palette color has the list of all palette colors sorted by
//!    distance from it. Scanning the list of the best color so far (the pivot), any color at
//!    least as close to `px` as the best lies within `d(pivot, px) + sqrt(best)` of the pivot,
//!    so the scan stops at that distance; whenever a closer color turns up, the scan switches to
//!    its list. About 10 distances per search instead of 256. Pillow's `Quant.c` (MIT-CMU)
//!    uses the same sorted tables (non-switching), and Phillips' sort-means (ALENEX 2002) the
//!    same idea for k-means.
//! 3. Small palettes, and inputs outside the range the rounding analysis covers, are searched
//!    by brute force.
//!
//! The sorted lists are built lazily, on the first search that uses a color as pivot, because
//! a searcher is built for every Voronoi iteration and every remap: images with few colors or
//! a good guess rate never pay for the `O(n^2 log n)` table.

use std::sync::OnceLock;

use crate::pal::FPixel;

/// The color that the difference blends on besides black, as premultiplied (r, g, b) at
/// alpha 1. If the channels are put through a linear transform `T` (an opponent color space,
/// say), this becomes `T * (1, 1, 1)`.
const WHITE: [f64; 3] = [1.0, 1.0, 1.0];

/// Maps a color to coordinates whose squared Euclidean distance is the color difference.
///
/// `FPixel::diff` adds the squared differences of the two colors blended on black (the
/// premultiplied channels `c`) and blended on white (`c + (1 - a) * w` with `w` the white
/// vector). With `dc = c1 - c2` and `da = a1 - a2`:
///
/// ```text
/// diff = |dc|^2 + |dc - da * w|^2
///      = 2 * |dc - da * w / 2|^2 + da^2 * |w|^2 / 2
/// ```
///
/// so `embed(c, a) = (sqrt(2) * (c - a * w / 2), a * |w| / sqrt(2))`, a rank-4 linear map
/// (for `w = (1, 1, 1)` the last coordinate is `sqrt(1.5) * a`). This is the only place that
/// defines the metric of the Improved search; `WHITE` and the channels may change, as long as
/// the result stays a linear map of `(a, r, g, b)`.
#[inline(always)]
pub(super) fn embed(px: FPixel) -> [f64; 4] {
    let [wr, wg, wb] = WHITE;
    let (a, s) = (f64::from(px.a), std::f64::consts::SQRT_2);
    let alpha_scale = ((wr * wr + wg * wg + wb * wb) / 2.0).sqrt();
    [
        s * (f64::from(px.r) - a * wr / 2.0),
        s * (f64::from(px.g) - a * wg / 2.0),
        s * (f64::from(px.b) - a * wb / 2.0),
        a * alpha_scale,
    ]
}

/// Squared Euclidean distance between embedded colors.
#[inline(always)]
pub(super) fn dist(p: &[f64; 4], q: &[f64; 4]) -> f64 {
    let d0 = p[0] - q[0];
    let d1 = p[1] - q[1];
    let d2 = p[2] - q[2];
    let d3 = p[3] - q[3];
    (d0 * d0 + d1 * d1) + (d2 * d2 + d3 * d3)
}

/// Penalty for making holes in IE (2.4.1): matching an opaque pixel to a translucent color.
const IE_PENALTY: f64 = 1.0 / 1024.0;

/// Palettes up to this size are searched by brute force (after the guess shortcut).
const BRUTE_FORCE_COLORS: usize = 16;

/// Largest embedded coordinate of a palette color for the fast paths. Pixels are within about
/// `[0, 1.03]`; the bound only excludes garbage, which the brute force handles consistently.
const MAX_COORD: f64 = 1e3;

/// Queries farther than this (squared) from the guess are searched by brute force, so that
/// every coordinate in the fast paths is below `MAX_COORD + sqrt(MAX_QUERY_DIFF) = 2e3`.
/// (Diffs are below about 7 for colors within `0..=1.03`.)
const MAX_QUERY_DIFF: f64 = 1e6;

/// Rounding slack of [`dist`], relative and absolute, in units of distance (not squared).
///
/// The fast paths only see coordinates below `3e3` (see [`MAX_QUERY_DIFF`]; the guess shortcut
/// only accepts queries within `2 * MAX_COORD` of the guess). There, the rounding of each
/// coordinate (three operations) and of each difference errs by less than
/// `4 * 2^-53 * 3e3 < 1.4e-12` absolute, so the computed distance is within `3e-12` of the
/// exact Euclidean distance between the exact images of the `f32` colors under the (rounded)
/// linear map; the squares and sums add a relative `4e-16`. `1e-9` leaves a margin of 300x,
/// which also absorbs the rounding of the bounds themselves.
const SLACK: f64 = 1e-9;

/// Lower bound of the exact distance (not squared), given a computed squared distance.
/// Monotonic in `c`.
#[inline(always)]
fn dist_lo(c: f64) -> f64 {
    let lo = c.sqrt() * (1.0 - SLACK) - SLACK;
    if lo > 0.0 { lo } else { 0.0 }
}

/// Upper bound of the exact distance (not squared), given a computed squared distance.
/// Monotonic in `c`.
#[inline(always)]
fn dist_hi(c: f64) -> f64 {
    c.sqrt() * (1.0 + SLACK) + SLACK
}

/// Packs a lower bound of a distance and a palette index into one sortable `u32`.
///
/// For non-negative floats, the bit patterns sort like the values. The low 8 bits of the
/// pattern are replaced by the index, which rounds the bound down (by at most `2^-15`
/// relative), so it stays a lower bound, stays monotonic, and ties sort by index.
#[inline(always)]
fn pack(lo: f64, index: usize) -> u32 {
    // rounding to f32 errs by 6e-8 relative, covered by the 1e-6 factor
    let lo = (lo * (1.0 - 1e-6)) as f32;
    (lo.to_bits() & !0xFF) | index as u32
}

#[inline(always)]
fn unpack(key: u32) -> (f64, usize) {
    (f64::from(f32::from_bits(key & !0xFF)), (key & 0xFF) as usize)
}

/// Exact nearest-color search, see the module documentation.
pub(crate) struct ExactNearest {
    /// Palette colors, embedded.
    points: Vec<[f64; 4]>,
    /// Colors with alpha below 1, which get the IE penalty.
    translucent: Vec<bool>,
    /// A guess whose computed difference is below this is certainly the strict nearest color.
    shortcut: Vec<f64>,
    /// Row `i`: all palette indices sorted by distance from color `i`, packed with lower
    /// bounds of the distances by [`pack`]. Empty when only brute force is used.
    rows: Vec<OnceLock<Box<[u32]>>>,
}

impl ExactNearest {
    pub fn new(colors: &[FPixel]) -> Self {
        let n = colors.len();
        assert!(n > 0, "palette must not be empty");
        let points: Vec<[f64; 4]> = colors.iter().map(|&c| embed(c)).collect();
        let translucent = colors.iter().map(|c| c.a < 1.0).collect();

        // The rounding analysis needs moderate finite coordinates; rows hold 8-bit indices.
        let in_range = points.iter().flatten().all(|v| v.abs() <= MAX_COORD);
        if !in_range || n > 256 {
            return Self {
                points,
                translucent,
                shortcut: vec![f64::NEG_INFINITY; n],
                rows: Vec::new(),
            };
        }

        let mut nearest_other = vec![f64::INFINITY; n];
        for i in 0..n {
            for j in i + 1..n {
                let d = dist(&points[i], &points[j]);
                nearest_other[i] = nearest_other[i].min(d);
                nearest_other[j] = nearest_other[j].min(d);
            }
        }
        // If the guess g is within m/2 of px, where m bounds the distance from g to every other
        // color from below, then by the triangle inequality every other color is farther than
        // m/2 from px. In bounds: dist_hi(guess) < m/2 implies that any color j with a computed
        // difference <= the guess's would have m - dist_hi(guess) <= dist_hi(j) <= dist_hi(guess)
        // (dist_hi is monotonic), a contradiction. Solving dist_hi(guess) < m/2 for the squared
        // difference, with an extra SLACK for the rounding of dist_hi:
        let shortcut = nearest_other
            .iter()
            .map(|&d| {
                let half = dist_lo(d) / 2.0 - 2.0 * SLACK;
                if d.is_infinite() {
                    f64::INFINITY // a single color
                } else if half > 0.0 {
                    let r = half / (1.0 + SLACK);
                    r * r
                } else {
                    f64::NEG_INFINITY
                }
            })
            .collect();
        let rows = if n > BRUTE_FORCE_COLORS {
            (0..n).map(|_| OnceLock::new()).collect()
        } else {
            Vec::new()
        };
        Self {
            points,
            translucent,
            shortcut,
            rows,
        }
    }

    /// Returns the index of the closest palette color, and its difference (see the module
    /// documentation for the exact definition).
    #[inline(always)]
    pub fn search(&self, px: FPixel, likely_colormap_index: usize, min_opaque_val: f32) -> (usize, f32) {
        let iebug = px.a > min_opaque_val;
        let n = self.points.len();
        let guess = if likely_colormap_index < n {
            likely_colormap_index
        } else {
            0
        };
        let q = embed(px);
        let guess_diff = dist(&q, &self.points[guess]);
        // with the penalty on the guess, a slightly farther opaque color can win
        if guess_diff < self.shortcut[guess] && !(iebug && self.translucent[guess]) {
            return (guess, guess_diff as f32);
        }
        if self.rows.is_empty() || guess_diff.is_nan() || guess_diff > MAX_QUERY_DIFF {
            return self.brute_force(&q, iebug);
        }
        self.orchard(&q, iebug, guess, guess_diff)
    }

    #[inline(always)]
    fn penalty(&self, j: usize, iebug: bool) -> f64 {
        if iebug && self.translucent[j] { IE_PENALTY } else { 0.0 }
    }

    /// The definition of the result: the first minimum of the penalized difference.
    #[inline(never)]
    pub(super) fn brute_force(&self, q: &[f64; 4], iebug: bool) -> (usize, f32) {
        let mut best = 0;
        let mut best_diff = dist(q, &self.points[0]) + self.penalty(0, iebug);
        for (j, p) in self.points.iter().enumerate().skip(1) {
            let d = dist(q, p) + self.penalty(j, iebug);
            if d < best_diff {
                best_diff = d;
                best = j;
            }
        }
        (best, best_diff as f32)
    }

    /// Row `i` of the sorted distance table, built on first use.
    fn row(&self, i: usize) -> &[u32] {
        self.rows[i].get_or_init(|| {
            let p = &self.points[i];
            let mut row: Box<[u32]> = self
                .points
                .iter()
                .enumerate()
                .map(|(j, c)| pack(dist_lo(dist(p, c)), j))
                .collect();
            row.sort_unstable();
            row
        })
    }

    /// Switching Orchard search starting from the guess. Requires finite moderate inputs
    /// (`guess_diff <= MAX_QUERY_DIFF`, palette coordinates within `MAX_COORD`).
    #[inline(never)]
    fn orchard(&self, q: &[f64; 4], iebug: bool, guess: usize, guess_diff: f64) -> (usize, f32) {
        // Invariant: every color evaluated so far is in `seen`, and `best` is the first minimum
        // of the penalized difference among them.
        let mut seen = [0u64; 4];
        seen[guess / 64] |= 1 << (guess % 64);
        let mut best = guess;
        let mut best_diff = guess_diff + self.penalty(guess, iebug);
        let mut pivot = guess;
        let mut pivot_hi = dist_hi(guess_diff);
        'pivot: loop {
            // A color j whose penalized difference is <= best_diff has an unpenalized difference
            // <= best_diff, so d(px, j) <= dist_hi(best_diff), and by the triangle inequality
            // d(pivot, j) <= d(pivot, px) + d(px, j) <= limit. The row is sorted by lower bounds
            // of d(pivot, j), so every such color comes before the first entry above the limit.
            // best_diff only shrinks by switching pivots, so the limit holds for the whole scan.
            let limit = pivot_hi + dist_hi(best_diff);
            for &key in self.row(pivot) {
                let (lo, j) = unpack(key);
                if lo > limit {
                    break 'pivot;
                }
                let bit = 1u64 << (j % 64);
                if seen[j / 64] & bit != 0 {
                    continue;
                }
                seen[j / 64] |= bit;
                let du = dist(q, &self.points[j]);
                let d = du + self.penalty(j, iebug);
                if d < best_diff {
                    best_diff = d;
                    best = j;
                    pivot = j;
                    pivot_hi = dist_hi(du);
                    continue 'pivot;
                }
                if d == best_diff && j < best {
                    best = j;
                }
            }
            break;
        }
        (best, best_diff as f32)
    }
}
