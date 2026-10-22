//! Image quality metrics for a quantized image against its original.
//!
//! Alpha: both images are composited over a flat gray background in sRGB-encoded space
//! (as browsers do), and every metric reports the worse of two backgrounds, dark gray 0.1
//! and light gray 0.9. This follows ssimulacra2's reference tool, and catches errors that
//! one background hides (for example a wrong color under nearly transparent pixels only
//! shows on one of them). Opaque images use a single 0.5 background, which has no effect.
//!
//! All sums are over fixed chunks combined in order, so scores are identical for any
//! thread count.

use libimgquant::RGBA;
use rayon::prelude::*;

/// Pixels per parallel work item. Fixed, so floating-point sums do not depend on threads.
const CHUNK: usize = 1 << 14;

/// Which expensive metrics to compute.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub butteraugli: bool,
}

/// Scores for one image (the worst over the compositing backgrounds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scores {
    /// SSIMULACRA 2.1, higher is better (100 = identical, 90 = visually lossless, 70 = high quality).
    pub ssimu2: f64,
    /// Butteraugli 3-norm (libjxl `--pnorm`), lower is better. NaN when skipped.
    pub butter_p3: f64,
    /// Butteraugli max-norm, lower is better. NaN when skipped.
    pub butter_max: f64,
    /// Mean CIEDE2000 color difference (about 1 = just noticeable), lower is better.
    pub de2000: f64,
    /// Mean OKLab Euclidean difference times 100, lower is better.
    pub de_ok: f64,
    /// 99th percentile of the OKLab difference times 100, lower is better.
    pub de_ok_p99: f64,
    /// Mean OKLab difference times 100 after a Gaussian blur (sigma 1 px, linear light),
    /// lower is better. A crude spatial model of the eye: dithering that averages out
    /// to the right color scores well, unlike per-pixel differences.
    pub de_ok_blur: f64,
    /// PSNR of the composited sRGB values, in dB. A sanity check only: it ranks
    /// perceptual improvements backwards.
    pub psnr: f64,
}

impl Scores {
    /// Combines two backgrounds' scores, keeping the worse value of each metric.
    fn worst(self, o: Self) -> Self {
        // f64::max/min ignore NaN, which only appears for skipped metrics.
        Self {
            ssimu2: self.ssimu2.min(o.ssimu2),
            butter_p3: self.butter_p3.max(o.butter_p3),
            butter_max: self.butter_max.max(o.butter_max),
            de2000: self.de2000.max(o.de2000),
            de_ok: self.de_ok.max(o.de_ok),
            de_ok_p99: self.de_ok_p99.max(o.de_ok_p99),
            de_ok_blur: self.de_ok_blur.max(o.de_ok_blur),
            psnr: self.psnr.min(o.psnr),
        }
    }
}

/// sRGB transfer function (IEC 61966-2-1), encoded 0..1 to linear 0..1.
#[must_use]
pub fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// OKLab from linear sRGB (Björn Ottosson, "A perceptual color space for image
/// processing", 2020; matrices from that page).
#[must_use]
pub fn oklab(lin: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = lin;
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn oklab_distance(x: [f32; 3], y: [f32; 3]) -> f32 {
    ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
}

/// CIELAB (D65 white) from linear sRGB, via the IEC 61966-2-1 sRGB to XYZ matrix.
#[must_use]
pub fn cielab(lin: [f32; 3]) -> [f64; 3] {
    let [r, g, b] = lin.map(f64::from);
    let x = (0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b) / 0.950_47;
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175 * b;
    let z = (0.019_333_9 * r + 0.119_192 * g + 0.950_304_1 * b) / 1.088_83;
    // CIE 15 cube root with the linear segment below (6/29)^3
    let f = |t: f64| {
        if t > 216.0 / 24389.0 {
            t.cbrt()
        } else {
            (24389.0 / 27.0 * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIEDE2000 color difference with kL = kC = kH = 1, implemented as in Sharma, Wu and
/// Dalal, "The CIEDE2000 color-difference formula: implementation notes, supplementary
/// test data, and mathematical observations", Color Res. Appl. 30(1):21-30, 2005
/// (equations 2-22, including their notes on hue angles of achromatic colors and on
/// the mean hue across 0/360 degrees).
#[must_use]
pub fn ciede2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    let [l1, a1, b1] = lab1;
    let [l2, a2, b2] = lab2;
    let pow25_7 = 25f64.powi(7);
    let c1 = a1.hypot(b1);
    let c2 = a2.hypot(b2);
    let c_bar7 = ((c1 + c2) / 2.0).powi(7);
    let g = 0.5 * (1.0 - (c_bar7 / (c_bar7 + pow25_7)).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = a1p.hypot(b1);
    let c2p = a2p.hypot(b2);
    // hue in degrees 0..360; 0 for achromatic colors (eq. 7)
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            b.atan2(a).to_degrees().rem_euclid(360.0)
        }
    };
    let h1p = hue(b1, a1p);
    let h2p = hue(b2, a2p);
    let chroma_product = c1p * c2p;

    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh = if chroma_product == 0.0 {
        0.0
    } else {
        let d = h2p - h1p;
        if d > 180.0 {
            d - 360.0
        } else if d < -180.0 {
            d + 360.0
        } else {
            d
        }
    };
    let dh_big = 2.0 * chroma_product.sqrt() * (dh / 2.0).to_radians().sin();

    let l_bar = (l1 + l2) / 2.0;
    let c_bar_p = (c1p + c2p) / 2.0;
    let h_bar = if chroma_product == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (h_bar - 30.0).to_radians().cos()
        + 0.24 * (2.0 * h_bar).to_radians().cos()
        + 0.32 * (3.0 * h_bar + 6.0).to_radians().cos()
        - 0.20 * (4.0 * h_bar - 63.0).to_radians().cos();
    let d_theta = 30.0 * (-((h_bar - 275.0) / 25.0).powi(2)).exp();
    let c_bar_p7 = c_bar_p.powi(7);
    let r_c = 2.0 * (c_bar_p7 / (c_bar_p7 + pow25_7)).sqrt();
    let l50 = (l_bar - 50.0).powi(2);
    let s_l = 1.0 + 0.015 * l50 / (20.0 + l50).sqrt();
    let s_c = 1.0 + 0.045 * c_bar_p;
    let s_h = 1.0 + 0.015 * c_bar_p * t;
    let r_t = -(2.0 * d_theta).to_radians().sin() * r_c;
    let (tl, tc, th) = (dl / s_l, dc / s_c, dh_big / s_h);
    (tl * tl + tc * tc + th * th + r_t * tc * th).sqrt()
}

/// Lookup tables for one background: composited sRGB value and its linear value, for
/// every (alpha, channel value) pair.
struct Composite {
    encoded: Vec<f32>,
    linear: Vec<f32>,
}

impl Composite {
    fn new(background: f64) -> Self {
        let mut encoded = Vec::with_capacity(256 * 256);
        let mut linear = Vec::with_capacity(256 * 256);
        for a in 0..256 {
            let a = f64::from(a) / 255.0;
            for v in 0..256 {
                let e = f64::from(v) / 255.0 * a + background * (1.0 - a);
                encoded.push(e as f32);
                linear.push(srgb_to_linear(e) as f32);
            }
        }
        Self { encoded, linear }
    }

    fn encoded(&self, px: RGBA) -> [f32; 3] {
        let a = usize::from(px.a) << 8;
        [px.r, px.g, px.b].map(|v| self.encoded[a | usize::from(v)])
    }

    fn linear(&self, px: RGBA) -> [f32; 3] {
        let a = usize::from(px.a) << 8;
        [px.r, px.g, px.b].map(|v| self.linear[a | usize::from(v)])
    }
}

/// Normalized Gaussian kernel, truncated at 3 sigma.
fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil() as i32;
    let k: Vec<f32> = (-radius..=radius)
        .map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = k.iter().sum();
    k.into_iter().map(|v| v / sum).collect()
}

/// Separable Gaussian blur of an RGB image, edges clamped.
#[must_use]
pub fn gaussian_blur(img: &[[f32; 3]], width: usize, height: usize, sigma: f32) -> Vec<[f32; 3]> {
    let k = gaussian_kernel(sigma);
    let r = k.len() / 2;
    let mut horiz = vec![[0.0f32; 3]; img.len()];
    horiz
        .par_chunks_mut(width)
        .zip(img.par_chunks(width))
        .for_each(|(out, row)| {
            for (x, o) in out.iter_mut().enumerate() {
                let mut acc = [0.0f32; 3];
                for (j, &kv) in k.iter().enumerate() {
                    let p = row[(x + j).saturating_sub(r).min(width - 1)];
                    for c in 0..3 {
                        acc[c] += p[c] * kv;
                    }
                }
                *o = acc;
            }
        });
    let mut out = vec![[0.0f32; 3]; img.len()];
    out.par_chunks_mut(width).enumerate().for_each(|(y, out)| {
        for (j, &kv) in k.iter().enumerate() {
            let sy = (y + j).saturating_sub(r).min(height - 1);
            for (o, p) in out.iter_mut().zip(&horiz[sy * width..(sy + 1) * width]) {
                for c in 0..3 {
                    o[c] += p[c] * kv;
                }
            }
        }
    });
    out
}

/// Sums `f` over `0..n` in fixed chunks, in parallel, combining the chunks in order.
fn chunked_sum(n: usize, f: impl Fn(usize) -> f64 + Sync) -> f64 {
    let partial: Vec<f64> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| (c * CHUNK..((c + 1) * CHUNK).min(n)).map(&f).sum())
        .collect();
    partial.iter().sum()
}

/// Nearest-rank percentile (`q` in 0..=1).
fn percentile(values: &mut [f32], q: f64) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    let rank = ((q * values.len() as f64).ceil() as usize).clamp(1, values.len()) - 1;
    *values.select_nth_unstable_by(rank, f32::total_cmp).1
}

fn score_background(
    original: &[RGBA],
    palette: &[RGBA],
    indices: &[u8],
    width: usize,
    height: usize,
    background: f64,
    opts: Options,
) -> Scores {
    let lut = Composite::new(background);
    let n = original.len();
    // The output has at most 256 distinct colors, so convert the palette once.
    let pal_enc: Vec<[f32; 3]> = palette.iter().map(|&p| lut.encoded(p)).collect();
    let pal_lin: Vec<[f32; 3]> = palette.iter().map(|&p| lut.linear(p)).collect();
    let pal_ok: Vec<[f32; 3]> = pal_lin.iter().map(|&p| oklab(p)).collect();
    let pal_lab: Vec<[f64; 3]> = pal_lin.iter().map(|&p| cielab(p)).collect();

    let orig_lin: Vec<[f32; 3]> = original.par_iter().map(|&p| lut.linear(p)).collect();
    let out_lin: Vec<[f32; 3]> = indices.par_iter().map(|&i| pal_lin[usize::from(i)]).collect();

    // SSIMULACRA2 is the slowest metric and mostly sequential, so run it alongside the rest.
    let (ssimu2, rest) = rayon::join(
        || ssimulacra2_score(orig_lin.clone(), out_lin.clone(), width, height),
        || {
            let sq_err = chunked_sum(n, |i| {
                let a = lut.encoded(original[i]);
                let b = pal_enc[usize::from(indices[i])];
                (0..3).map(|c| (f64::from(a[c] - b[c]) * 255.0).powi(2)).sum::<f64>()
            });
            let mse = sq_err / (3 * n) as f64;
            let psnr = if mse > 0.0 {
                10.0 * (255.0 * 255.0 / mse).log10()
            } else {
                f64::INFINITY
            };

            let mut de: Vec<f32> = orig_lin
                .par_iter()
                .zip(indices.par_iter())
                .map(|(&o, &i)| oklab_distance(oklab(o), pal_ok[usize::from(i)]) * 100.0)
                .collect();
            let de_ok = chunked_sum(n, |i| f64::from(de[i])) / n as f64;
            let de_ok_p99 = f64::from(percentile(&mut de, 0.99));
            drop(de);

            let de2000 =
                chunked_sum(n, |i| ciede2000(cielab(orig_lin[i]), pal_lab[usize::from(indices[i])])) / n as f64;

            let de_ok_blur = {
                let ob = gaussian_blur(&orig_lin, width, height, 1.0);
                let db = gaussian_blur(&out_lin, width, height, 1.0);
                chunked_sum(n, |i| f64::from(oklab_distance(oklab(ob[i]), oklab(db[i])) * 100.0)) / n as f64
            };

            let (butter_p3, butter_max) = if opts.butteraugli {
                butteraugli_scores(&orig_lin, &out_lin, width, height)
            } else {
                (f64::NAN, f64::NAN)
            };
            Scores {
                ssimu2: f64::NAN,
                butter_p3,
                butter_max,
                de2000,
                de_ok,
                de_ok_p99,
                de_ok_blur,
                psnr,
            }
        },
    );
    Scores { ssimu2, ..rest }
}

fn ssimulacra2_score(a: Vec<[f32; 3]>, b: Vec<[f32; 3]>, width: usize, height: usize) -> f64 {
    match (
        ssimulacra2::LinearRgb::new(a, width, height),
        ssimulacra2::LinearRgb::new(b, width, height),
    ) {
        (Ok(a), Ok(b)) => ssimulacra2::compute_frame_ssimulacra2(a, b).unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

#[cfg(feature = "butteraugli")]
fn butteraugli_scores(a: &[[f32; 3]], b: &[[f32; 3]], width: usize, height: usize) -> (f64, f64) {
    use butteraugli::{ButteraugliParams, Img, ImgVec, RGB};
    let to_img = |v: &[[f32; 3]]| -> ImgVec<RGB<f32>> {
        Img::new(v.iter().map(|&[r, g, b]| RGB::new(r, g, b)).collect(), width, height)
    };
    let (a, b) = (to_img(a), to_img(b));
    match butteraugli::butteraugli_linear(a.as_ref(), b.as_ref(), &ButteraugliParams::default()) {
        Ok(r) => (r.pnorm_3, r.score),
        Err(_) => (f64::NAN, f64::NAN),
    }
}

#[cfg(not(feature = "butteraugli"))]
fn butteraugli_scores(_: &[[f32; 3]], _: &[[f32; 3]], _: usize, _: usize) -> (f64, f64) {
    (f64::NAN, f64::NAN)
}

/// Scores `palette[indices]` against `original` (both `width` x `height`).
#[must_use]
pub fn measure(
    original: &[RGBA],
    palette: &[RGBA],
    indices: &[u8],
    width: usize,
    height: usize,
    opts: Options,
) -> Scores {
    assert_eq!(original.len(), width * height);
    assert_eq!(indices.len(), width * height);
    let has_alpha = original.iter().any(|p| p.a != 255) || palette.iter().any(|p| p.a != 255);
    let backgrounds: &[f64] = if has_alpha { &[0.1, 0.9] } else { &[0.5] };
    // the backgrounds run in parallel; collecting first keeps the combination order fixed
    let scores: Vec<Scores> = backgrounds
        .par_iter()
        .map(|&bg| score_background(original, palette, indices, width, height, bg, opts))
        .collect();
    scores
        .into_iter()
        .reduce(Scores::worst)
        .expect("at least one background")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sharma, Wu, Dalal (2005), table 1: all 34 published test pairs (L1 a1 b1, L2 a2 b2, dE00),
    /// from https://hajim.rochester.edu/ece/sites/gsharma/ciede2000/dataNprograms/ciede2000testdata.txt
    const SHARMA_PAIRS: [([f64; 3], [f64; 3], f64); 34] = [
        ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
        ([50.0, 3.1571, -77.2803], [50.0, 0.0, -82.7485], 2.8615),
        ([50.0, 2.8361, -74.0200], [50.0, 0.0, -82.7485], 3.4412),
        ([50.0, -1.3802, -84.2814], [50.0, 0.0, -82.7485], 1.0000),
        ([50.0, -1.1848, -84.8006], [50.0, 0.0, -82.7485], 1.0000),
        ([50.0, -0.9009, -85.5211], [50.0, 0.0, -82.7485], 1.0000),
        ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
        ([50.0, -1.0, 2.0], [50.0, 0.0, 0.0], 2.3669),
        ([50.0, 2.49, -0.001], [50.0, -2.49, 0.0009], 7.1792),
        ([50.0, 2.49, -0.001], [50.0, -2.49, 0.0010], 7.1792),
        ([50.0, 2.49, -0.001], [50.0, -2.49, 0.0011], 7.2195),
        ([50.0, 2.49, -0.001], [50.0, -2.49, 0.0012], 7.2195),
        ([50.0, -0.001, 2.49], [50.0, 0.0009, -2.49], 4.8045),
        ([50.0, -0.001, 2.49], [50.0, 0.0010, -2.49], 4.8045),
        ([50.0, -0.001, 2.49], [50.0, 0.0011, -2.49], 4.7461),
        ([50.0, 2.5, 0.0], [50.0, 0.0, -2.5], 4.3065),
        ([50.0, 2.5, 0.0], [73.0, 25.0, -18.0], 27.1492),
        ([50.0, 2.5, 0.0], [61.0, -5.0, 29.0], 22.8977),
        ([50.0, 2.5, 0.0], [56.0, -27.0, -3.0], 31.9030),
        ([50.0, 2.5, 0.0], [58.0, 24.0, 15.0], 19.4535),
        ([50.0, 2.5, 0.0], [50.0, 3.1736, 0.5854], 1.0000),
        ([50.0, 2.5, 0.0], [50.0, 3.2972, 0.0], 1.0000),
        ([50.0, 2.5, 0.0], [50.0, 1.8634, 0.5757], 1.0000),
        ([50.0, 2.5, 0.0], [50.0, 3.2592, 0.3350], 1.0000),
        ([60.2574, -34.0099, 36.2677], [60.4626, -34.1751, 39.4387], 1.2644),
        ([63.0109, -31.0961, -5.8663], [62.8187, -29.7946, -4.0864], 1.2630),
        ([61.2901, 3.7196, -5.3901], [61.4292, 2.2480, -4.9620], 1.8731),
        ([35.0831, -44.1164, 3.7933], [35.0232, -40.0716, 1.5901], 1.8645),
        ([22.7233, 20.0904, -46.6940], [23.0331, 14.9730, -42.5619], 2.0373),
        ([36.4612, 47.8580, 18.3852], [36.2715, 50.5065, 21.2231], 1.4146),
        ([90.8027, -2.0831, 1.4410], [91.1528, -1.6435, 0.0447], 1.4441),
        ([90.9257, -0.5406, -0.9208], [88.6381, -0.8985, -0.7239], 1.5381),
        ([6.7747, -0.2908, -2.4247], [5.8714, -0.0985, -2.2286], 0.6377),
        ([2.0776, 0.0795, -1.1350], [0.9033, -0.0636, -0.5514], 0.9082),
    ];

    #[test]
    fn ciede2000_sharma_test_data() {
        for (i, &(a, b, expected)) in SHARMA_PAIRS.iter().enumerate() {
            let got = ciede2000(a, b);
            assert!((got - expected).abs() < 5e-5, "pair {}: {got} != {expected}", i + 1);
            // the formula is symmetric
            assert!((ciede2000(b, a) - got).abs() < 1e-9, "pair {} not symmetric", i + 1);
        }
        assert_eq!(ciede2000([50.0, 10.0, -10.0], [50.0, 10.0, -10.0]), 0.0);
    }

    #[test]
    fn cielab_reference_colors() {
        let close = |x: [f64; 3], y: [f64; 3]| x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 0.01);
        let white = cielab([1.0, 1.0, 1.0]);
        assert!(close(white, [100.0, 0.0, 0.0]), "{white:?}");
        // sRGB primaries (Lindbloom's calculator, D65)
        let red = cielab([1.0, 0.0, 0.0]);
        assert!(close(red, [53.2408, 80.0925, 67.2032]), "{red:?}");
        let blue = cielab([0.0, 0.0, 1.0]);
        assert!(close(blue, [32.2970, 79.1875, -107.8602]), "{blue:?}");
        assert!(close(cielab([0.0; 3]), [0.0; 3]));
    }

    #[test]
    fn oklab_reference_colors() {
        let close = |x: [f32; 3], y: [f32; 3]| x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-3);
        assert!(close(oklab([1.0, 1.0, 1.0]), [1.0, 0.0, 0.0]));
        // CSS Color 4 examples: red = oklab(0.628 0.225 0.126), blue = oklab(0.452 -0.032 -0.312)
        assert!(close(oklab([1.0, 0.0, 0.0]), [0.627_96, 0.224_86, 0.125_85]));
        assert!(close(oklab([0.0, 0.0, 1.0]), [0.452_01, -0.032_46, -0.311_53]));
    }

    #[test]
    fn srgb_transfer() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-12);
        // both branches meet at the threshold
        let t = 0.040_45;
        assert!((srgb_to_linear(t) - ((t + 0.055) / 1.055f64).powf(2.4)).abs() < 1e-7);
        assert!((srgb_to_linear(0.5) - 0.214_041).abs() < 1e-6);
    }

    #[test]
    fn blur_preserves_constant_and_mass() {
        let (w, h) = (9, 7);
        let flat = vec![[0.25f32, 0.5, 0.75]; w * h];
        for p in gaussian_blur(&flat, w, h, 1.0) {
            for c in 0..3 {
                assert!((p[c] - flat[0][c]).abs() < 1e-6);
            }
        }
        // an impulse away from the edges spreads symmetrically and keeps its total
        let mut img = vec![[0.0f32; 3]; w * h];
        img[3 * w + 4] = [1.0; 3];
        let b = gaussian_blur(&img, w, h, 1.0);
        let total: f32 = b.iter().map(|p| p[0]).sum();
        assert!((total - 1.0).abs() < 1e-5);
        assert!((b[3 * w + 3][0] - b[3 * w + 5][0]).abs() < 1e-7);
        assert!((b[2 * w + 4][0] - b[4 * w + 4][0]).abs() < 1e-7);
        let k = gaussian_kernel(1.0);
        assert_eq!(k.len(), 7);
        assert!((b[3 * w + 4][0] - k[3] * k[3]).abs() < 1e-6);
    }

    #[test]
    fn percentile_nearest_rank() {
        let mut v: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        assert_eq!(percentile(&mut v, 0.99), 99.0);
        assert_eq!(percentile(&mut v, 1.0), 100.0);
        assert_eq!(percentile(&mut [5.0], 0.99), 5.0);
    }

    fn test_image(w: usize, h: usize, alpha: bool) -> Vec<RGBA> {
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                RGBA::new(
                    (x * 7) as u8,
                    (y * 5) as u8,
                    ((x + y) * 3) as u8,
                    if alpha { (x * 4) as u8 } else { 255 },
                )
            })
            .collect()
    }

    #[test]
    fn identical_image_scores_perfect() {
        for alpha in [false, true] {
            // 16x16 pixels, used as their own 256-color palette
            let px = test_image(16, 16, alpha);
            let indices: Vec<u8> = (0..=255).collect();
            let s = measure(&px, &px, &indices, 16, 16, Options { butteraugli: true });
            assert_eq!(s.de_ok, 0.0);
            assert_eq!(s.de_ok_p99, 0.0);
            assert_eq!(s.de2000, 0.0);
            assert_eq!(s.de_ok_blur, 0.0);
            assert_eq!(s.psnr, f64::INFINITY);
            assert!((s.ssimu2 - 100.0).abs() < 1e-6, "ssimulacra2 {}", s.ssimu2);
            if cfg!(feature = "butteraugli") {
                assert!(s.butter_max < 1e-3, "butteraugli {}", s.butter_max);
            }
        }
    }

    #[test]
    fn worse_output_scores_worse() {
        let (w, h) = (32, 32);
        let px = test_image(w, h, false);
        // two palettes: 16 grays from the image's luma vs a single gray
        let luma = |p: &RGBA| ((u32::from(p.r) * 3 + u32::from(p.g) * 6 + u32::from(p.b)) / 10) as u8;
        let fine: Vec<RGBA> = (0..16).map(|i| RGBA::new(i * 17, i * 17, i * 17, 255)).collect();
        let fine_idx: Vec<u8> = px.iter().map(|p| (luma(p) + 8) / 17).collect();
        let coarse = [RGBA::new(128, 128, 128, 255)];
        let coarse_idx = vec![0u8; w * h];
        let opts = Options { butteraugli: false };
        let a = measure(&px, &fine, &fine_idx, w, h, opts);
        let b = measure(&px, &coarse, &coarse_idx, w, h, opts);
        assert!(a.ssimu2 > b.ssimu2);
        assert!(a.de2000 < b.de2000);
        assert!(a.de_ok < b.de_ok);
        assert!(a.de_ok_blur < b.de_ok_blur);
        assert!(a.psnr > b.psnr);
        assert!(a.butter_p3.is_nan());
    }

    #[test]
    fn transparent_errors_count_on_one_background() {
        // A fully transparent pixel's color never shows. A half-transparent black
        // replaced by half-transparent white differs on both backgrounds.
        let (w, h) = (16, 16);
        let px = vec![RGBA::new(0, 0, 0, 0); w * h];
        let s = measure(
            &px,
            &[RGBA::new(255, 255, 255, 0)],
            &vec![0; w * h],
            w,
            h,
            Options { butteraugli: false },
        );
        assert_eq!(s.de_ok, 0.0);
        let px = vec![RGBA::new(0, 0, 0, 128); w * h];
        let s = measure(
            &px,
            &[RGBA::new(255, 255, 255, 128)],
            &vec![0; w * h],
            w,
            h,
            Options { butteraugli: false },
        );
        assert!(s.de_ok > 10.0);
    }
}
