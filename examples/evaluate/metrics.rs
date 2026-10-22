//! Image quality metrics for judging quantizer changes.
//!
//! Alpha is handled by compositing both images over black and over white (in sRGB, as
//! browsers do) and averaging the metric over the two backgrounds.

use libimgquant::RGBA;

pub struct Metrics {
    /// PSNR of RGB, in dB.
    pub psnr: f64,
    /// Mean SSIM of luma (Wang et al. 2004), reported as `1 - SSIM`; lower is better.
    pub dssim: f64,
    /// Mean color difference in OKLab, times 100 (about 1 = just noticeable).
    pub delta_e: f64,
    /// 99th percentile of the OKLab difference, times 100.
    pub delta_e_p99: f64,
}

const BACKGROUNDS: [f32; 2] = [0.0, 255.0];

fn composite(px: RGBA, bg: f32) -> [f32; 3] {
    let a = f32::from(px.a) / 255.0;
    [
        f32::from(px.r) * a + bg * (1.0 - a),
        f32::from(px.g) * a + bg * (1.0 - a),
        f32::from(px.b) * a + bg * (1.0 - a),
    ]
}

fn srgb_to_linear(c: f32) -> f32 {
    let c = c / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Björn Ottosson's OKLab (2020).
fn oklab(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(srgb_to_linear);
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn luma(rgb: [f32; 3]) -> f32 {
    0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2]
}

fn gaussian_kernel() -> [f32; 11] {
    let mut k = [0.0f32; 11];
    for (i, v) in k.iter_mut().enumerate() {
        let x = i as f32 - 5.0;
        *v = (-(x * x) / (2.0 * 1.5 * 1.5)).exp();
    }
    let sum: f32 = k.iter().sum();
    k.map(|v| v / sum)
}

/// Separable 11x11 Gaussian filter, "valid" region only (as in Wang's reference ssim.m).
fn filter_valid(src: &[f32], w: usize, h: usize, k: &[f32; 11]) -> (Vec<f32>, usize, usize) {
    let (ow, oh) = (w - 10, h - 10);
    let mut horiz = vec![0.0f32; ow * h];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        for x in 0..ow {
            horiz[y * ow + x] = k.iter().zip(&row[x..x + 11]).map(|(a, b)| a * b).sum();
        }
    }
    let mut out = vec![0.0f32; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            out[y * ow + x] = (0..11).map(|i| k[i] * horiz[(y + i) * ow + x]).sum();
        }
    }
    (out, ow, oh)
}

/// Box-downsamples by `f`, as Wang's reference implementation does for large images.
fn downsample(src: &[f32], w: usize, h: usize, f: usize) -> (Vec<f32>, usize, usize) {
    if f <= 1 {
        return (src.to_vec(), w, h);
    }
    let (ow, oh) = (w / f, h / f);
    let mut out = vec![0.0f32; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            let mut sum = 0.0;
            for dy in 0..f {
                for dx in 0..f {
                    sum += src[(y * f + dy) * w + x * f + dx];
                }
            }
            out[y * ow + x] = sum / (f * f) as f32;
        }
    }
    (out, ow, oh)
}

fn ssim(x: &[f32], y: &[f32], w: usize, h: usize) -> Option<f64> {
    let f = ((w.min(h) as f64 / 256.0).round() as usize).max(1);
    let (x, w2, h2) = downsample(x, w, h, f);
    let (y, _, _) = downsample(y, w, h, f);
    let (w, h) = (w2, h2);
    if w < 11 || h < 11 {
        return None;
    }
    let k = gaussian_kernel();
    let xx: Vec<f32> = x.iter().map(|v| v * v).collect();
    let yy: Vec<f32> = y.iter().map(|v| v * v).collect();
    let xy: Vec<f32> = x.iter().zip(&y).map(|(a, b)| a * b).collect();
    let (mx, _, _) = filter_valid(&x, w, h, &k);
    let (my, _, _) = filter_valid(&y, w, h, &k);
    let (sxx, _, _) = filter_valid(&xx, w, h, &k);
    let (syy, _, _) = filter_valid(&yy, w, h, &k);
    let (sxy, _, _) = filter_valid(&xy, w, h, &k);
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let mut total = 0.0f64;
    for i in 0..mx.len() {
        let (mx, my) = (f64::from(mx[i]), f64::from(my[i]));
        let vx = f64::from(sxx[i]) - mx * mx;
        let vy = f64::from(syy[i]) - my * my;
        let cxy = f64::from(sxy[i]) - mx * my;
        total += ((2.0 * mx * my + c1) * (2.0 * cxy + c2)) / ((mx * mx + my * my + c1) * (vx + vy + c2));
    }
    Some(total / mx.len() as f64)
}

pub fn measure(original: &[RGBA], palette: &[RGBA], indices: &[u8], width: usize, height: usize) -> Metrics {
    let n = original.len();
    let mut sq_err = 0.0f64;
    let mut dssim_sum = 0.0f64;
    let mut dssim_count = 0;
    let mut deltas = Vec::with_capacity(n * BACKGROUNDS.len());

    for bg in BACKGROUNDS {
        let mut lx = Vec::with_capacity(n);
        let mut ly = Vec::with_capacity(n);
        for (&o, &i) in original.iter().zip(indices) {
            let a = composite(o, bg);
            let b = composite(palette[usize::from(i)], bg);
            for c in 0..3 {
                sq_err += f64::from(a[c] - b[c]).powi(2);
            }
            let (la, lb) = (oklab(a), oklab(b));
            let d = ((la[0] - lb[0]).powi(2) + (la[1] - lb[1]).powi(2) + (la[2] - lb[2]).powi(2)).sqrt();
            deltas.push(d * 100.0);
            lx.push(luma(a));
            ly.push(luma(b));
        }
        if let Some(s) = ssim(&lx, &ly, width, height) {
            dssim_sum += 1.0 - s;
            dssim_count += 1;
        }
    }

    let mse = sq_err / (n * 3 * BACKGROUNDS.len()) as f64;
    let psnr = if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    let delta_e = deltas.iter().map(|&d| f64::from(d)).sum::<f64>() / deltas.len() as f64;
    deltas.sort_unstable_by(f32::total_cmp);
    let delta_e_p99 = f64::from(deltas[(deltas.len() * 99 / 100).min(deltas.len() - 1)]);
    Metrics {
        psnr,
        dssim: if dssim_count > 0 {
            dssim_sum / f64::from(dssim_count)
        } else {
            0.0
        },
        delta_e,
        delta_e_p99,
    }
}
