//! The 2.4.1 vantage-point search (`Profile::Compat`), restructured for speed with
//! bit-identical results.
//!
//! The heads, radii, candidate lists, candidate order and tie rule are exactly those of
//! libimagequant 2.4.1 (`build_head` is unchanged). What differs is how they are evaluated:
//!
//! - **Layout.** Vantage points and candidates are stored as structure-of-arrays, with alpha
//!   kept in `f32` (2.4.1 subtracts alpha in `f32`) and r, g, b pre-widened to `f64` (exact).
//!   Distances are evaluated [`LANES`] at a time with exactly the operations of
//!   [`FPixel::diff`] in the same order, so LLVM can vectorize them without changing a bit.
//! - **Pruning of vantage-point tests.** `d = sqrt(FPixel::diff)` is a metric (see
//!   `exact.rs`), so when the guess shortcut fails, the triangle inequality bounds
//!   `d(px, vp) >= |d(guess, vp) - d(guess, px)|` (Orchard 1991; Vidal's AESA 1986).
//!   A chunk of vantage points is skipped only when every one of them certainly fails the
//!   2.4.1 radius test, so the first head that passes is still found by the exact test.
//! - **Pruning of candidates.** A head's candidates are sorted by distance from its vantage
//!   point, so `d(px, c) >= |d(vp, c) - d(px, vp)|` limits the scan to an annulus, as in
//!   vantage-point trees. Only candidates certainly worse than one already seen are skipped,
//!   so the first minimum is the same as in the full scan.
//!
//! All bounds allow for the rounding of `FPixel::diff` (see [`sqrt_bounds`]). Inputs that are
//! not finite take a scalar path that is the 2.4.1 code, so the result is identical for every
//! input. The original implementation is kept in `reference.rs` and compared against this one
//! by a randomized test; together these cut the distance evaluations per search by about 2x.
//!
//! Vantage points: Yianilos, "Data structures and algorithms for nearest neighbor search in
//! general metric spaces", SODA 1993.

use std::cmp::Ordering;

use crate::pal::{FPixel, MAX_DIFF, PalF, cmin};

/// Distances computed per step. 4 was measured as fast as 8.
const LANES: usize = 4;

/// Lower and upper bounds of `sqrt(e)` (the exact metric distance), given a finite computed
/// difference `c`. Both are monotonic in `c`.
///
/// Rounding of `FPixel::diff`: with `e` the exact value of the formula for the `f32` inputs,
/// the computed `c` satisfies `|c - e| <= REL * e + ABS`:
/// - the alpha difference is rounded to `f32`: with `al = a2 - a1` it errs by at most
///   `2^-24 * |al|`; per channel `black^2 + white^2 >= al^2 / 2` (because `white - black = al`),
///   so `al^2 <= 2e/3` and `sum |white| <= sqrt(3e)`, and the error of the sum of squares is at
///   most `2 * 2^-24 * |al| * sum |white| <= 2^-23 * sqrt(2) * e`, about `1.7e-7 * e`;
/// - the `f64` arithmetic adds about `1e-15 * e`; the final rounding to `f32` adds
///   `2^-24 * e`, or `2^-150` absolute for subnormal results.
///
/// That is `2.3e-7 * e + 2^-150`, so `REL = 1e-6` and `ABS = 1e-30` have a margin of 4x and
/// more.
///
/// Then `sqrt(e) <= (sqrt(c) + sqrt(ABS)) * (1 + REL)` and
/// `sqrt(e) >= sqrt(c) * (1 - REL) - sqrt(ABS)`, with `sqrt(ABS) = 1e-15`. The `f32` square
/// root and the two operations after it each round by at most `6e-8` relative, so the factors
/// `1 -+ 4e-6` and the terms `-+ 2e-15` leave a margin of more than 2x.
#[inline(always)]
fn sqrt_bounds(c: f32) -> (f32, f32) {
    let s = c.sqrt();
    (s * (1.0 - 4e-6) - 2e-15, s * (1.0 + 4e-6) + 2e-15)
}

/// True if a computed difference `FPixel::diff(px, c)` is certainly greater than `limit`,
/// given a lower bound `lb` of the exact `sqrt(e)`, computed in `f32` from the bounds of
/// [`sqrt_bounds`] with one subtraction.
///
/// That subtraction rounds `lb` up by at most a factor `1 + 6e-8`; `lb * lb` and the product
/// with `1 - 1e-5` add `1.8e-7`, so the left side is below `e * (1 - 9.7e-6)`, while the
/// computed difference is at least `e * (1 - 2.3e-7) - 2^-150`. The absolute `1e-30` covers
/// tiny and subnormal limits. Padded heads have `limit = -inf` and always fail.
#[inline(always)]
fn certainly_outside(lb: f32, limit: f32) -> bool {
    lb * lb * (1.0 - 1e-5) > limit + 1e-30
}

/// 2.4.1 skips a head if `diff > radius`, so a NaN difference selects it.
#[inline(always)]
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn selects_head(diff: f32, radius: f32) -> bool {
    !(diff > radius)
}

/// Structure-of-arrays colors. `a` stays `f32` because 2.4.1 subtracts alpha in `f32`;
/// widening r, g, b to `f64` is exact.
#[derive(Default)]
struct Soa {
    a: Vec<f32>,
    r: Vec<f64>,
    g: Vec<f64>,
    b: Vec<f64>,
}

impl Soa {
    fn push(&mut self, c: FPixel) {
        self.a.push(c.a);
        self.r.push(f64::from(c.r));
        self.g.push(f64::from(c.g));
        self.b.push(f64::from(c.b));
    }

    /// Pads to a multiple of [`LANES`] with copies of `fill`.
    fn pad(&mut self, fill: FPixel) {
        while self.a.len() % LANES != 0 {
            self.push(fill);
        }
    }

    fn len(&self) -> usize {
        self.a.len()
    }

    /// The color as it was pushed (narrowing the widened channels back is exact).
    fn get(&self, i: usize) -> FPixel {
        FPixel {
            a: self.a[i],
            r: self.r[i] as f32,
            g: self.g[i] as f32,
            b: self.b[i] as f32,
        }
    }
}

/// `px.diff(c)` for [`LANES`] colors at once, with the operations of `FPixel::diff` in the same
/// order (the sum is `(r + g) + b`). Rust never contracts these into fused multiply-adds.
#[inline(always)]
fn diff_lanes(px: &FPixel, pr: f64, pg: f64, pb: f64, s: &Soa, base: usize) -> [f32; LANES] {
    let a: &[f32; LANES] = s.a[base..base + LANES].try_into().expect("padded");
    let r: &[f64; LANES] = s.r[base..base + LANES].try_into().expect("padded");
    let g: &[f64; LANES] = s.g[base..base + LANES].try_into().expect("padded");
    let b: &[f64; LANES] = s.b[base..base + LANES].try_into().expect("padded");
    let mut out = [0f32; LANES];
    for l in 0..LANES {
        let alphas = f64::from(a[l] - px.a);
        let br = pr - r[l];
        let wr = br + alphas;
        let bg = pg - g[l];
        let wg = bg + alphas;
        let bb = pb - b[l];
        let wb = bb + alphas;
        out[l] = ((br * br + wr * wr) + (bg * bg + wg * wg) + (bb * bb + wb * wb)) as f32;
    }
    out
}

/// One candidate scan: the query and the head's candidates.
struct Scan<'a> {
    px: FPixel,
    pr: f64,
    pg: f64,
    pb: f64,
    cand: &'a Soa,
    start: usize,
    transparent: &'a [bool],
    penalize: bool,
}

impl Scan<'_> {
    /// Updates the per-lane minima with candidates `j..j + LANES` (relative to the head).
    /// `EARLIER` is set when chunks are visited in decreasing order, so that equal values move
    /// to the earlier candidate.
    #[inline(always)]
    fn visit<const EARLIER: bool>(&self, j: usize, best: &mut [f32; LANES], best_j: &mut [u32; LANES]) {
        let mut d = diff_lanes(&self.px, self.pr, self.pg, self.pb, self.cand, self.start + j);
        if self.penalize {
            // penalty for making holes in IE
            for (d, &transparent) in d.iter_mut().zip(&self.transparent[j..j + LANES]) {
                if transparent {
                    *d += 1.0 / 1024.0;
                }
            }
        }
        for l in 0..LANES {
            if d[l] < best[l] || (EARLIER && d[l] == best[l]) {
                best[l] = d[l];
                best_j[l] = (j + l) as u32;
            }
        }
    }
}

struct Head {
    /// Colors less than `radius` away from `vantage_point` have their best match in candidates.
    vantage_point: FPixel,
    radius: f32,
    candidates_color: Vec<FPixel>,
    candidates_index: Vec<u16>,
}

/// The 2.4.1 vantage-point search, see the module documentation.
pub(crate) struct VpNearest {
    colors: Vec<FPixel>,
    nearest_other_color_dist: Vec<f32>,
    /// Vantage points of all heads except the fallback, padded with heads that never match.
    vp: Soa,
    vp_radius: Vec<f32>,
    num_heads: usize,
    /// Candidates of every head (the fallback last), each list padded to [`LANES`] with NaN
    /// colors, whose distance is NaN and so never compares smaller.
    cand: Soa,
    cand_index: Vec<u16>,
    cand_transparent: Vec<bool>,
    /// Bounds of the distance (not squared) from each candidate to its head's vantage point,
    /// non-decreasing within a head because candidates are sorted by that distance; `+inf` in
    /// padding.
    cand_vp_lo: Vec<f32>,
    cand_vp_hi: Vec<f32>,
    cand_start: Vec<u32>,
    cand_len: Vec<u32>,
    any_transparent: bool,
    /// Bounds of `sqrt(diff(colors[i], vantage_point[h]))` at `[i * vp.len() + h]`.
    tri_lo: Vec<f32>,
    tri_hi: Vec<f32>,
    /// All palette colors and vantage points are finite (else only the scalar path is used).
    finite: bool,
}

fn build_head(
    px: FPixel,
    colors: &[FPixel],
    num_candidates: usize,
    error_margin: f32,
    skip_index: &mut [bool],
    skipped: &mut usize,
) -> Head {
    // colors in skip_index have been eliminated already in previous heads
    let mut sorted: Vec<(f32, usize)> = colors
        .iter()
        .enumerate()
        .filter(|&(i, _)| !skip_index[i])
        .map(|(i, c)| (px.diff(c), i))
        .collect();
    // stable sort, closest first; ties keep palette order
    sorted.sort_by(|a, b| {
        if a.0 > b.0 {
            Ordering::Greater
        } else if a.0 < b.0 {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    });

    let num_candidates = cmin(sorted.len(), num_candidates);
    let candidates = &sorted[..num_candidates];
    let mut head = Head {
        vantage_point: px,
        radius: 0.0,
        candidates_color: candidates.iter().map(|&(_, i)| colors[i]).collect(),
        candidates_index: candidates.iter().map(|&(_, i)| i as u16).collect(),
    };
    let Some(&(_, farthest)) = candidates.last() else {
        return head;
    };

    // if all colors within this radius are included in candidates, then there cannot be any other better match
    // farther away from the vantage point than half of the radius. Due to alpha channel must assume pessimistic radius.
    head.radius = px.min_diff(&colors[farthest]) / 4.0; // /4 = half of radius, but radius is squared

    for &(radius, i) in candidates {
        // divide again as that's matching certain subset within radius-limited subset
        if radius < head.radius / 4.0 - error_margin {
            skip_index[i] = true;
            *skipped += 1;
        }
    }
    head
}

/// Half of the squared distance from each color to its nearest other color, as 2.4.1
/// computes it. `diff` is symmetric bit for bit (IEEE rounding is symmetric under negation,
/// and every difference just changes sign), so each pair is computed once. The minimum of
/// the same values (ignoring NaN, never `-0.0`) doesn't depend on the order they come in.
fn nearest_other_color_dist(colors: &[FPixel]) -> Vec<f32> {
    let n = colors.len();
    let mut second_best = vec![MAX_DIFF as f32; n];
    for i in 0..n {
        for j in i + 1..n {
            let diff = colors[i].diff(&colors[j]);
            if diff <= second_best[i] {
                second_best[i] = diff;
            }
            if diff <= second_best[j] {
                second_best[j] = diff;
            }
        }
    }
    // half of squared distance
    second_best.iter().map(|&d| d / 4.0).collect()
}

fn is_finite(c: &FPixel) -> bool {
    c.a.is_finite() && c.r.is_finite() && c.g.is_finite() && c.b.is_finite()
}

impl VpNearest {
    pub fn new(map: &PalF, fast: bool) -> Self {
        let colors: Vec<FPixel> = map.items.iter().map(|p| p.color).collect();
        let n = colors.len();
        assert!(n > 0, "palette must not be empty");

        let subset: Vec<FPixel> = match &map.subset {
            Some(s) => s.iter().map(|p| p.color).collect(),
            None => colors[..n.div_ceil(4)].to_vec(),
        };
        let num_vantage_points = if n > 16 {
            cmin(n / if fast { 4 } else { 3 }, subset.len())
        } else {
            0
        };

        let nearest_other_color_dist = nearest_other_color_dist(&colors);

        let mut skip_index = vec![false; n];
        let mut skipped = 0;
        // floats and colordifference calculations are not perfect
        let error_margin = if fast { 0.0 } else { 8.0f32 / 256.0 / 256.0 };

        let mut heads = Vec::with_capacity(num_vantage_points + 1);
        for (h, &vantage_point) in subset.iter().enumerate().take(num_vantage_points) {
            let num_candidates = 1 + (n - skipped) / ((1 + num_vantage_points - h) / 2);
            let head = build_head(
                vantage_point,
                &colors,
                num_candidates,
                error_margin,
                &mut skip_index,
                &mut skipped,
            );
            if head.candidates_index.is_empty() {
                break;
            }
            heads.push(head);
        }

        // assumption that there is no better color within radius of vantage point color
        // holds true only for colors within convex hull formed by palette colors.
        // The fallback must contain all colors, since there are too many edge cases to cover.
        if !fast {
            skip_index.fill(false);
        }
        let mut fallback = build_head(
            FPixel::default(),
            &colors,
            n,
            error_margin,
            &mut skip_index,
            &mut skipped,
        );
        if fallback.candidates_index.is_empty() {
            // the original read past the end here; search everything instead
            skip_index.fill(false);
            fallback = build_head(
                FPixel::default(),
                &colors,
                n,
                error_margin,
                &mut skip_index,
                &mut skipped,
            );
        }
        fallback.radius = MAX_DIFF as f32;
        heads.push(fallback);

        let num_heads = heads.len() - 1;
        let mut vp = Soa::default();
        let mut vp_radius = Vec::with_capacity(num_heads + LANES);
        for h in &heads[..num_heads] {
            vp.push(h.vantage_point);
            vp_radius.push(h.radius);
        }
        // `diff > -inf` for any finite query: padded heads never match
        vp.pad(FPixel::default());
        vp_radius.resize(vp.len(), f32::NEG_INFINITY);

        let mut cand = Soa::default();
        let mut cand_index = Vec::new();
        let mut cand_transparent = Vec::new();
        let mut cand_vp_lo = Vec::new();
        let mut finite_vp_dist = true;
        let mut cand_vp_hi = Vec::new();
        let mut cand_start = Vec::with_capacity(heads.len());
        let mut cand_len = Vec::with_capacity(heads.len());
        for h in &heads {
            cand_start.push(cand.len() as u32);
            cand_len.push(h.candidates_color.len() as u32);
            for (&c, &i) in h.candidates_color.iter().zip(&h.candidates_index) {
                cand.push(c);
                cand_index.push(i);
                cand_transparent.push(c.a < 1.0);
                // the same value build_head sorted by
                let diff = h.vantage_point.diff(&c);
                finite_vp_dist &= diff.is_finite();
                let (lo, hi) = sqrt_bounds(diff);
                cand_vp_lo.push(lo);
                cand_vp_hi.push(hi);
            }
            cand.pad(FPixel::splat(f32::NAN));
            cand_index.resize(cand.len(), 0);
            cand_transparent.resize(cand.len(), false);
            cand_vp_lo.resize(cand.len(), f32::INFINITY);
            cand_vp_hi.resize(cand.len(), f32::INFINITY);
        }
        let any_transparent = colors.iter().any(|c| c.a < 1.0);
        // the bounds assume finite differences
        let mut finite =
            finite_vp_dist && colors.iter().all(is_finite) && heads.iter().all(|h| is_finite(&h.vantage_point));

        let hp = vp.len();
        let mut tri_lo = vec![0f32; n * hp];
        let mut tri_hi = vec![f32::INFINITY; n * hp];
        if finite {
            for (i, c) in colors.iter().enumerate() {
                for (h, head) in heads[..num_heads].iter().enumerate() {
                    let diff = c.diff(&head.vantage_point);
                    // the bounds assume a finite difference
                    finite &= diff.is_finite();
                    let (lo, hi) = sqrt_bounds(diff);
                    tri_lo[i * hp + h] = lo;
                    tri_hi[i * hp + h] = hi;
                }
            }
        }

        Self {
            colors,
            nearest_other_color_dist,
            vp,
            vp_radius,
            num_heads,
            cand,
            cand_index,
            cand_transparent,
            cand_vp_lo,
            cand_vp_hi,
            cand_start,
            cand_len,
            any_transparent,
            tri_lo,
            tri_hi,
            finite,
        }
    }

    /// Returns the index of the closest palette color, and its difference.
    #[inline(always)]
    pub fn search(&self, px: FPixel, likely_colormap_index: usize, min_opaque_val: f32) -> (usize, f32) {
        let iebug = px.a > min_opaque_val;

        let likely = if likely_colormap_index < self.colors.len() {
            likely_colormap_index
        } else {
            0
        };
        let guess_diff = self.colors[likely].diff(&px);
        if guess_diff < self.nearest_other_color_dist[likely] {
            return (likely, guess_diff);
        }
        // With a finite palette, the guess difference is finite exactly when the query is finite
        // and far from overflowing, which the vectorized path assumes.
        if self.finite && guess_diff.is_finite() {
            self.search_heads(px, iebug, likely, guess_diff)
        } else {
            self.search_heads_scalar(px, iebug)
        }
    }

    #[inline(never)]
    fn search_heads(&self, px: FPixel, iebug: bool, guess: usize, guess_diff: f32) -> (usize, f32) {
        let (pr, pg, pb) = (f64::from(px.r), f64::from(px.g), f64::from(px.b));

        // First head whose ball contains px; the fallback (index num_heads) always matches.
        // Its vantage point is transparent black.
        let mut head = self.num_heads;
        let mut vp_diff = None;
        let (dlo, dhi) = sqrt_bounds(guess_diff);
        let hp = self.vp.len();
        let tlo = &self.tri_lo[guess * hp..(guess + 1) * hp];
        let thi = &self.tri_hi[guess * hp..(guess + 1) * hp];
        let mut base = 0;
        while base < self.num_heads {
            let rad: &[f32; LANES] = self.vp_radius[base..base + LANES].try_into().expect("padded");
            let lo: &[f32; LANES] = tlo[base..base + LANES].try_into().expect("padded");
            let hi: &[f32; LANES] = thi[base..base + LANES].try_into().expect("padded");
            // triangle inequality: d(px, vp) >= |d(guess, vp) - d(guess, px)|
            let mut all_outside = true;
            for l in 0..LANES {
                let lb = (lo[l] - dhi).max(dlo - hi[l]).max(0.0);
                all_outside &= certainly_outside(lb, rad[l]);
            }
            if !all_outside {
                let d = diff_lanes(&px, pr, pg, pb, &self.vp, base);
                if let Some(l) = (0..LANES).find(|&l| selects_head(d[l], rad[l])) {
                    head = base + l;
                    vp_diff = Some(d[l]);
                    break;
                }
            }
            base += LANES;
        }
        let vp_diff = vp_diff.unwrap_or_else(|| px.diff(&FPixel::default()));
        self.scan_candidates(px, iebug, head, vp_diff)
    }

    /// The first minimum of the (penalized) difference among the head's candidates, as the
    /// 2.4.1 sequential scan finds it, skipping candidates that certainly can't match it.
    ///
    /// Candidates are sorted by their distance `D` from the vantage point, and
    /// `d(px, c) >= |D(c) - d(px, vp)|`. So the scan starts at candidates with `D` near
    /// `d(px, vp)` and goes outward in both directions, stopping in each direction once the
    /// bound certainly exceeds the best difference found so far (as in vantage-point trees).
    /// A skipped candidate has a larger difference than one already seen, so it can be neither
    /// the minimum nor tied with it, and the result is that of the full scan.
    #[inline(always)]
    fn scan_candidates(&self, px: FPixel, iebug: bool, head: usize, vp_diff: f32) -> (usize, f32) {
        let start = self.cand_start[head] as usize;
        let len = self.cand_len[head] as usize;
        let padded = len.next_multiple_of(LANES);
        let s = Scan {
            px,
            pr: f64::from(px.r),
            pg: f64::from(px.g),
            pb: f64::from(px.b),
            cand: &self.cand,
            start,
            transparent: &self.cand_transparent[start..start + padded],
            penalize: iebug && self.any_transparent,
        };

        // Per lane, the minimum and its lowest candidate position. Distances are never NaN here
        // (finite inputs) except in padding, which never compares smaller or equal. Candidate 0
        // is in lane 0, so if no difference is below infinity, the result is candidate 0, as in
        // the sequential scan.
        let mut best = [f32::INFINITY; LANES];
        let mut best_j = [0u32; LANES];
        let vp_lo = &self.cand_vp_lo[start..start + padded];
        let vp_hi = &self.cand_vp_hi[start..start + padded];
        let (tlo, thi) = sqrt_bounds(vp_diff);
        // the smallest difference seen so far
        let bound = |best: &[f32; LANES]| best[0].min(best[1]).min(best[2].min(best[3]));
        // the chunk where D crosses d(px, vp); a linear walk, because it is usually near the start
        let mut mid = 0;
        while mid + LANES < len && vp_lo[mid + LANES] < tlo {
            mid += LANES;
        }
        // Outward, in increasing positions (strict `<` keeps the first of equal values):
        // D only grows, and so does the bound D - d(px, vp) of the chunk's first lane.
        let mut j = mid;
        while j < padded && !certainly_outside((vp_lo[j] - thi).max(0.0), bound(&best)) {
            s.visit::<false>(j, &mut best, &mut best_j);
            j += LANES;
        }
        // Inward, in decreasing positions (`<=` moves to the earlier of equal values):
        // D only shrinks, and the bound d(px, vp) - D of the chunk's last lane grows.
        let mut j = mid;
        while j > 0 && !certainly_outside((tlo - vp_hi[j - 1]).max(0.0), bound(&best)) {
            j -= LANES;
            s.visit::<true>(j, &mut best, &mut best_j);
        }

        // smallest difference; ties go to the earliest candidate, as in the sequential scan
        let mut bd = best[0];
        let mut bj = best_j[0];
        for l in 1..LANES {
            if best[l] < bd || (best[l] == bd && best_j[l] < bj) {
                bd = best[l];
                bj = best_j[l];
            }
        }
        (usize::from(self.cand_index[start + bj as usize]), bd)
    }

    /// The 2.4.1 head walk, unchanged, for inputs that are not finite.
    #[cold]
    fn search_heads_scalar(&self, px: FPixel, iebug: bool) -> (usize, f32) {
        let mut head = self.num_heads;
        for h in 0..self.num_heads {
            if selects_head(px.diff(&self.vp.get(h)), self.vp_radius[h]) {
                head = h;
                break;
            }
        }
        let start = self.cand_start[head] as usize;
        let len = self.cand_len[head] as usize;
        let dist_to = |j: usize| {
            let c = self.cand.get(start + j);
            let mut dist = px.diff(&c);
            // penalty for making holes in IE
            if iebug && c.a < 1.0 {
                dist += 1.0 / 1024.0;
            }
            dist
        };
        let mut ind = 0;
        let mut dist = dist_to(0);
        for j in 1..len {
            let newdist = dist_to(j);
            if newdist < dist {
                dist = newdist;
                ind = j;
            }
        }
        (usize::from(self.cand_index[start + ind]), dist)
    }
}
