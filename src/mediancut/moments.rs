//! The color difference as a quadratic form, and additive box statistics under it.
//!
//! [`FPixel::diff`] takes the worse of blending on black and on white. With premultiplied
//! alpha that is `Σ_c Δc² + (Δc − Δa)²` over c ∈ {r, g, b}: a positive definite quadratic
//! form `Δᵀ M Δ` in (a, r, g, b), i.e. the squared Euclidean distance after the linear
//! embedding [`EMBED`]. Means, variances and sums of squared errors under the metric
//! therefore follow from the weighted moments `Σw`, `Σw·x` and `Σw·x·xᵀ` (Wu, "Efficient
//! Statistical Computations for Optimal Color Quantization", Graphics Gems II, 1991).
//!
//! [`EMBED`] is the only place the improved median cut knows the metric: another linear
//! color difference (for example one with an opponent transform of the color channels)
//! only needs another matrix here.

use crate::hist::HistItem;
use crate::pal::FPixel;

/// Linear embedding of a premultiplied (a, r, g, b) color in which [`FPixel::diff`] is the
/// squared Euclidean distance. Rows: r, g, b (blended on black), r−a, g−a, b−a (on white).
pub(super) const EMBED: [[f64; 4]; 6] = [
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
    [-1.0, 1.0, 0.0, 0.0],
    [-1.0, 0.0, 1.0, 0.0],
    [-1.0, 0.0, 0.0, 1.0],
];

/// The metric `M = EMBEDᵀ·EMBED`: `diff(x, y) = (x − y)ᵀ M (x − y)`.
pub(super) const METRIC: [[f64; 4]; 4] = gram(&EMBED);

const fn gram(e: &[[f64; 4]; 6]) -> [[f64; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    let mut i = 0;
    while i < 4 {
        let mut j = 0;
        while j < 4 {
            let mut k = 0;
            while k < e.len() {
                m[i][j] += e[k][i] * e[k][j];
                k += 1;
            }
            j += 1;
        }
        i += 1;
    }
    m
}

/// Squarings of `C·M` when finding the principal axis (see [`Moments::split_direction`]).
const SQUARINGS: usize = 12;

/// Index pairs (i ≤ j) of the upper triangle of a symmetric 4×4 matrix, in storage order.
const UPPER: [(usize, usize); 10] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 2),
    (2, 3),
    (3, 3),
];

#[inline(always)]
pub(super) fn argb(c: FPixel) -> [f64; 4] {
    [f64::from(c.a), f64::from(c.r), f64::from(c.g), f64::from(c.b)]
}

/// Projection of a color on a split direction. Always computed by this one function, so a
/// key is bitwise the same wherever it is recomputed.
#[inline(always)]
pub(super) fn key(dir: &[f64; 4], c: FPixel) -> f64 {
    let x = argb(c);
    dir[0] * x[0] + dir[1] * x[1] + dir[2] * x[2] + dir[3] * x[3]
}

#[inline(always)]
fn mat_vec(m: &[[f64; 4]; 4], v: &[f64; 4]) -> [f64; 4] {
    let mut o = [0.0; 4];
    for (o, row) in o.iter_mut().zip(m) {
        *o = row[0] * v[0] + row[1] * v[1] + row[2] * v[2] + row[3] * v[3];
    }
    o
}

/// `METRIC = L·diag(D)·Lᵀ` with a unit lower-triangular L, for [`metric_norm`].
const LDL: ([[f64; 4]; 4], [f64; 4]) = ldl(&METRIC);

/// LDLᵀ decomposition of a symmetric positive definite matrix.
const fn ldl(m: &[[f64; 4]; 4]) -> ([[f64; 4]; 4], [f64; 4]) {
    let mut l = [[0.0; 4]; 4];
    let mut d = [0.0; 4];
    let mut j = 0;
    while j < 4 {
        let mut dj = m[j][j];
        let mut k = 0;
        while k < j {
            dj -= l[j][k] * l[j][k] * d[k];
            k += 1;
        }
        d[j] = dj;
        l[j][j] = 1.0;
        let mut i = j + 1;
        while i < 4 {
            let mut v = m[i][j];
            let mut k = 0;
            while k < j {
                v -= l[i][k] * l[j][k] * d[k];
                k += 1;
            }
            l[i][j] = v / dj;
            i += 1;
        }
        j += 1;
    }
    (l, d)
}

/// `xᵀ M x`, the squared length of the embedded vector, as `Σ D_k·(Lᵀx)_k²` (fewer operations
/// than the full product; it's evaluated for every candidate cut).
#[inline(always)]
fn metric_norm(x: &[f64; 4]) -> f64 {
    let (l, d) = &LDL;
    let y3 = x[3];
    let y2 = x[2] + l[3][2] * x[3];
    let y1 = x[1] + l[2][1] * x[2] + l[3][1] * x[3];
    let y0 = x[0] + l[1][0] * x[1] + l[2][0] * x[2] + l[3][0] * x[3];
    d[0] * y0 * y0 + d[1] * y1 * y1 + d[2] * y2 * y2 + d[3] * y3 * y3
}

/// `tr(M·A)` for a symmetric `A` stored as its upper triangle.
#[inline(always)]
fn metric_trace(a: &[f64; 10]) -> f64 {
    let mut t = 0.0;
    for (&(i, j), &v) in UPPER.iter().zip(a) {
        t += if i == j {
            METRIC[i][j] * v
        } else {
            2.0 * METRIC[i][j] * v
        };
    }
    t
}

/// Weighted moments of a set of histogram items, with `adjusted_weight` as the weight (so
/// the feedback loop still steers the split) plus the perceptual weight for popularity.
/// Sums are accumulated in a fixed order, so they're deterministic.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Moments {
    /// Σw
    pub w: f64,
    /// Σw·x for x = (a, r, g, b)
    pub s: [f64; 4],
    /// Σw·x·xᵀ, upper triangle in [`UPPER`] order
    pub xx: [f64; 10],
    /// Σ perceptual_weight
    pub pw: f64,
    pub count: usize,
    /// Number of fully opaque items, for the `min_opaque_val` rounding.
    pub opaque: usize,
}

impl Moments {
    #[inline(always)]
    pub fn add(&mut self, item: &HistItem) {
        let x = argb(item.color);
        let w = f64::from(item.adjusted_weight);
        self.w += w;
        let wx = [w * x[0], w * x[1], w * x[2], w * x[3]];
        for (s, wx) in self.s.iter_mut().zip(wx) {
            *s += wx;
        }
        for (xx, &(i, j)) in self.xx.iter_mut().zip(&UPPER) {
            *xx += wx[i] * x[j];
        }
        self.pw += f64::from(item.perceptual_weight);
        self.count += 1;
        // the same test as average_pixels
        self.opaque += usize::from(f64::from(item.color.a) >= 255.0 / 256.0);
    }

    pub fn of(items: &[HistItem]) -> Self {
        let mut m = Self::default();
        for item in items {
            m.add(item);
        }
        m
    }

    #[inline(always)]
    pub fn merge(&mut self, o: &Self) {
        self.w += o.w;
        for (a, b) in self.s.iter_mut().zip(&o.s) {
            *a += b;
        }
        for (a, b) in self.xx.iter_mut().zip(&o.xx) {
            *a += b;
        }
        self.pw += o.pw;
        self.count += o.count;
        self.opaque += o.opaque;
    }

    pub fn sum(parts: &[Self]) -> Self {
        let mut m = Self::default();
        for p in parts {
            m.merge(p);
        }
        m
    }

    /// The part needed to score a cut: Σw, Σw·x and Σw·xᵀMx.
    #[inline(always)]
    pub fn linear(&self) -> Linear {
        Linear {
            w: self.w,
            s: self.s,
            q: metric_trace(&self.xx),
        }
    }

    /// Weighted scatter matrix `Σw·(x − x̄)(x − x̄)ᵀ`.
    fn scatter(&self) -> [[f64; 4]; 4] {
        let mut c = [[0.0; 4]; 4];
        if self.w > 0.0 {
            for (&(i, j), &xx) in UPPER.iter().zip(&self.xx) {
                let v = xx - self.s[i] * self.s[j] / self.w;
                c[i][j] = v;
                c[j][i] = v;
            }
        }
        c
    }

    /// Weighted sum of squared errors around the mean, under the metric.
    #[cfg(test)]
    pub fn sse(&self) -> f64 {
        let c = self.scatter();
        let mut t = 0.0;
        for (mrow, crow) in METRIC.iter().zip(&c) {
            for (m, c) in mrow.iter().zip(crow) {
                t += m * c;
            }
        }
        t.max(0.0)
    }

    /// Representative color: the weighted mean, which is the color with least weighted error
    /// under the (quadratic) metric. Like `average_pixels`, if the box has an opaque color and
    /// its mean opacity is at least `min_opaque_val`, the color is made opaque, keeping its hue.
    pub fn color(&self, min_opaque_val: f32) -> FPixel {
        if self.w <= 0.0 {
            return FPixel::default();
        }
        let [mut a, mut r, mut g, mut b] = self.s.map(|v| v / self.w);
        if a >= f64::from(min_opaque_val) && self.opaque > 0 && a > 0.0 {
            // premultiplied mean / mean alpha = alpha-weighted mean of the unpremultiplied colors
            r /= a;
            g /= a;
            b /= a;
            a = 1.0;
        }
        FPixel {
            a: a as f32,
            r: r as f32,
            g: g as f32,
            b: b as f32,
        }
    }

    /// Direction `d` such that the key `d·x` orders colors along the box's principal axis
    /// under the metric (Orchard & Bouman, "Color Quantization of Images", IEEE Trans. Signal
    /// Processing 39(12), 1991; Wu, "Color quantization by dynamic programming and principal
    /// analysis", ACM TOG 11(4), 1992).
    ///
    /// In the embedding `y = E·x` the scatter is `E·C·Eᵀ`. Its principal eigenvector lies in
    /// the range of E, `u = E·v`, where `C·M·v = λ·v`. The projection `u·y = (M·v)·x`, so `d = M·v`.
    /// `C·M` is similar to a symmetric positive semi-definite matrix, so its powers converge to
    /// `λ₁ᵏ·v·wᵀ`. Repeated squaring reaches `(C·M)⁴⁰⁹⁶` in 12 matrix products, which converges
    /// even when the two largest eigenvalues are within a fraction of a percent; when they are
    /// equal, any vector of their span is a principal axis. The column with the largest norm is
    /// taken, which is deterministic.
    pub fn split_direction(&self) -> [f64; 4] {
        let c = self.scatter();
        let mut a = mat_mul(&c, &METRIC);
        for _ in 0..SQUARINGS {
            let scale = a.iter().flatten().fold(0.0f64, |m, v| m.max(v.abs()));
            if scale == 0.0 || !scale.is_finite() {
                break;
            }
            for v in a.iter_mut().flatten() {
                *v /= scale;
            }
            a = mat_mul(&a, &a);
        }
        let mut v = [0.0; 4];
        let mut best = 0.0;
        for col in (0..4).map(|j| a.map(|row| row[j])) {
            let n = col.iter().map(|x| x * x).sum::<f64>();
            if n > best {
                best = n;
                v = col.map(|x| x / n.sqrt());
            }
        }
        if best == 0.0 || !best.is_finite() {
            // no spread: any direction will do (the caller falls back to splitting by position)
            return [0.0, 1.0, 1.0, 1.0];
        }
        mat_vec(&METRIC, &v)
    }
}

#[inline(always)]
fn mat_mul(x: &[[f64; 4]; 4], y: &[[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut o = [[0.0; 4]; 4];
    for (o, x) in o.iter_mut().zip(x) {
        for (j, o) in o.iter_mut().enumerate() {
            *o = x[0] * y[0][j] + x[1] * y[1][j] + x[2] * y[2][j] + x[3] * y[3][j];
        }
    }
    o
}

/// Σw, Σw·x and Σw·xᵀMx: enough to compute the SSE of a set, and additive.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Linear {
    pub w: f64,
    pub s: [f64; 4],
    pub q: f64,
}

impl Linear {
    /// Moments of one item.
    #[inline(always)]
    pub fn of_item(item: &HistItem) -> Self {
        let x = argb(item.color);
        let w = f64::from(item.adjusted_weight);
        Self {
            w,
            s: x.map(|v| w * v),
            q: w * metric_norm(&x),
        }
    }

    #[inline(always)]
    pub fn add(&mut self, o: &Self) {
        self.w += o.w;
        for (a, b) in self.s.iter_mut().zip(&o.s) {
            *a += b;
        }
        self.q += o.q;
    }

    #[inline(always)]
    pub fn minus(&self, o: &Self) -> Self {
        Self {
            w: self.w - o.w,
            s: [
                self.s[0] - o.s[0],
                self.s[1] - o.s[1],
                self.s[2] - o.s[2],
                self.s[3] - o.s[3],
            ],
            q: self.q - o.q,
        }
    }

    /// `Σw·|y − ȳ|² = Σw·|y|² − |Σw·y|²/Σw` in the embedding.
    #[inline(always)]
    pub fn sse(&self) -> f64 {
        if self.w > 0.0 {
            self.q - metric_norm(&self.s) / self.w
        } else {
            0.0
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> f32 {
        let mut s = seed;
        move || {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (s >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    pub(crate) fn random_items(seed: u64, n: usize) -> Vec<HistItem> {
        let mut next = rng(seed);
        (0..n)
            .map(|_| {
                let a = if next() < 0.3 { next() } else { 1.0 };
                let w = 0.5 + 10.0 * next();
                HistItem {
                    color: FPixel {
                        a,
                        r: next() * a,
                        g: next() * a,
                        b: next() * a,
                    },
                    adjusted_weight: w,
                    perceptual_weight: w * 0.75,
                    ..HistItem::default()
                }
            })
            .collect()
    }

    #[test]
    fn metric_is_the_color_difference() {
        assert_eq!(
            METRIC,
            [
                [3.0, -1.0, -1.0, -1.0],
                [-1.0, 2.0, 0.0, 0.0],
                [-1.0, 0.0, 2.0, 0.0],
                [-1.0, 0.0, 0.0, 2.0]
            ]
        );
        let items = random_items(1, 200);
        for p in items.windows(2) {
            let (x, y) = (p[0].color, p[1].color);
            let (ax, ay) = (argb(x), argb(y));
            let d = [ax[0] - ay[0], ax[1] - ay[1], ax[2] - ay[2], ax[3] - ay[3]];
            let emb: f64 = EMBED
                .iter()
                .map(|row| {
                    let e = row[0] * d[0] + row[1] * d[1] + row[2] * d[2] + row[3] * d[3];
                    e * e
                })
                .sum();
            let diff = f64::from(x.diff(&y));
            assert!((diff - metric_norm(&d)).abs() <= 1e-6 * diff.max(1e-6), "{diff}");
            assert!((emb - metric_norm(&d)).abs() <= 1e-12);
        }
    }

    #[test]
    fn ldl_metric_norm_is_the_quadratic_form() {
        let (l, d) = &LDL;
        assert!(d.iter().all(|&d| d > 0.0), "positive definite");
        for i in 0..4 {
            for j in 0..4 {
                let lld: f64 = (0..4).map(|k| l[i][k] * d[k] * l[j][k]).sum();
                assert!((lld - METRIC[i][j]).abs() < 1e-12);
            }
        }
        for item in random_items(3, 100) {
            let x = argb(item.color);
            let full = x.iter().zip(mat_vec(&METRIC, &x)).map(|(a, b)| a * b).sum::<f64>();
            assert!((metric_norm(&x) - full).abs() <= 1e-12 * full.max(1.0));
        }
    }

    #[test]
    fn moments_give_the_brute_force_sse() {
        for seed in 0..20 {
            let items = random_items(seed, 1 + seed as usize * 13);
            let m = Moments::of(&items);
            let mean = m.color(2.0); // no opaque rounding
            let brute: f64 = items
                .iter()
                .map(|i| f64::from(i.adjusted_weight) * f64::from(mean.diff(&i.color)))
                .sum();
            let tol = 1e-5 * brute.max(1e-9);
            assert!((m.sse() - brute).abs() <= tol, "{} vs {brute}", m.sse());
            assert!(
                (m.linear().sse() - brute).abs() <= tol,
                "{} vs {brute}",
                m.linear().sse()
            );
            assert_eq!(m.count, items.len());
        }
    }

    #[test]
    fn merged_moments_equal_moments_of_the_union() {
        let items = random_items(7, 100);
        let mut m = Moments::of(&items[..37]);
        m.merge(&Moments::of(&items[37..]));
        let all = Moments::of(&items);
        assert_eq!(m.count, all.count);
        assert_eq!(m.opaque, all.opaque);
        assert!((m.sse() - all.sse()).abs() <= 1e-9 * all.sse());
        let (l, r) = (Moments::of(&items[..37]).linear(), all.linear().minus(&m.linear()));
        assert!(r.w.abs() < 1e-9 && r.q.abs() < 1e-9);
        assert!(l.w > 0.0);
    }

    #[test]
    fn mean_color_and_opaque_rounding() {
        let item = |a: f32, r: f32, w: f32| HistItem {
            color: FPixel {
                a,
                r: r * a,
                g: 0.0,
                b: 0.0,
            },
            adjusted_weight: w,
            perceptual_weight: w,
            ..HistItem::default()
        };
        let items = [item(1.0, 0.5, 3.0), item(0.5, 0.5, 1.0)];
        let m = Moments::of(&items);
        // premultiplied mean
        let c = m.color(1.0);
        assert!((c.a - 0.875).abs() < 1e-6 && (c.r - 0.4375).abs() < 1e-6);
        // rounded to opaque, keeping the unpremultiplied red
        let c = m.color(0.8);
        assert_eq!(c.a, 1.0);
        assert!((c.r - 0.5).abs() < 1e-6);
        // no opaque color: no rounding
        let m = Moments::of(&[item(0.9, 0.5, 1.0), item(0.95, 0.5, 1.0)]);
        assert!(m.color(0.5).a < 1.0);
    }

    #[test]
    fn split_direction_is_the_principal_axis() {
        for seed in 0..30 {
            let items = random_items(100 + seed, 3 + seed as usize * 5);
            let m = Moments::of(&items);
            let d = m.split_direction();
            // the variance of the keys along d, per unit of metric length of the axis, must be the
            // largest over all directions: compare with many random directions
            let c = m.scatter();
            // axis v = M⁻¹·d; spread along it in the embedding is (dᵀ C d) / (vᵀ M v) = (dᵀ C d) / (dᵀ v)
            let spread = |d: &[f64; 4]| {
                let cd = mat_vec(&c, d);
                let num = d.iter().zip(&cd).map(|(a, b)| a * b).sum::<f64>();
                // vᵀMv = dᵀ M⁻¹ d; solve M v = d with M⁻¹ known in closed form for this M
                let v = solve_metric(d);
                num / d.iter().zip(&v).map(|(a, b)| a * b).sum::<f64>()
            };
            let best = spread(&d);
            let mut next = rng(seed);
            for _ in 0..200 {
                let mut t = [0.0f64; 4];
                for v in &mut t {
                    *v = f64::from(next()) - 0.5;
                }
                let t = mat_vec(&METRIC, &t);
                assert!(spread(&t) <= best * (1.0 + 1e-6), "seed {seed}");
            }
        }
    }

    /// M⁻¹·d by Gaussian elimination (test only).
    fn solve_metric(d: &[f64; 4]) -> [f64; 4] {
        let mut a = METRIC;
        let mut b = *d;
        for i in 0..4 {
            for r in i + 1..4 {
                let f = a[r][i] / a[i][i];
                let pivot = a[i];
                for (x, p) in a[r].iter_mut().zip(pivot).skip(i) {
                    *x -= f * p;
                }
                b[r] -= f * b[i];
            }
        }
        let mut x = [0.0; 4];
        for i in (0..4).rev() {
            let mut s = b[i];
            for c in i + 1..4 {
                s -= a[i][c] * x[c];
            }
            x[i] = s / a[i][i];
        }
        x
    }
}
