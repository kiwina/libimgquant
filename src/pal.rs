//! Internal color representation.
//!
//! Arithmetic mirrors the mixed `f32`/`f64` precision of libimagequant 2.4.1 exactly,
//! because palette choices are sensitive to rounding.

use crate::RGBA;

pub(crate) const INTERNAL_GAMMA: f64 = 0.5499;
pub(crate) const MAX_DIFF: f64 = 1e20;
/// Avoid allocating buffers larger than 64MB.
pub(crate) const HIGH_MEMORY_LIMIT: usize = 1 << 26;

/// `if a > b { a } else { b }`, which differs from `f32::max` for NaN, as the original's `MAX` macro did.
#[inline(always)]
pub(crate) fn cmax<T: PartialOrd>(a: T, b: T) -> T {
    if a > b { a } else { b }
}

/// `if a < b { a } else { b }`, as the original's `MIN` macro.
#[inline(always)]
pub(crate) fn cmin<T: PartialOrd>(a: T, b: T) -> T {
    if a < b { a } else { b }
}

/// How 8-bit colors map to the internal coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorSpace {
    /// Each channel in internal gamma (2.4.1).
    Rgb,
    /// An opponent transform of the internal-gamma channels: lightness and two chroma
    /// axes, using the matrices of Björn Ottosson's OKLab ("A perceptual color space for
    /// image processing", 2020). OKLab applies them to linear light with a cube root in
    /// between; here they act on the 2.4.1 gamma values instead, which keeps the space
    /// linear in them, so premultiplication, blending on black and white, and every
    /// distance stay exactly as consistent as in 2.4.1. Equal RGB weights overstate
    /// errors in blue and understate lightness errors; this weighs them like OKLab does.
    ///
    /// Chroma is stored offset by 0.5 (before premultiplying) so that every channel stays
    /// within 0..1 like RGB, which the sort keys and dithering limits of 2.4.1 rely on. With
    /// the offset, [`FPixel::diff`] also adds `Δα²/2` per chroma channel, a mild extra
    /// weight on alpha differences.
    Opponent,
}

/// OKLab's M2 · M1. Rows sum to (1, 0, 0), so white maps to lightness 1 and no chroma.
const OPPONENT: [[f32; 3]; 3] = [
    [0.254_564_58, 0.651_941_56, 0.093_493_86],
    [0.340_534_8, -0.465_335_9, 0.124_801_12],
    [0.105_142_21, 0.318_906_37, -0.424_048_54],
];

/// Inverse of [`OPPONENT`].
const OPPONENT_INVERSE: [[f32; 3]; 3] = [
    [1.0, 1.944_265_1, 0.792_693],
    [1.0, -0.747_676_73, 0.000_431_470_8],
    [1.0, -0.080_213_76, -2.161_349],
];

const CHROMA_OFFSET: f32 = 0.5;

/// Converts 8-bit channels to internal coordinates for one gamma and color space.
#[derive(Clone)]
pub(crate) struct ColorLut {
    gamma: [f32; 256],
    space: ColorSpace,
}

impl ColorLut {
    pub fn new(gamma: f64, space: ColorSpace) -> Self {
        let mut lut = [0.0; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = (i as f64 / 255.0).powf(INTERNAL_GAMMA / gamma) as f32;
        }
        Self { gamma: lut, space }
    }

    pub fn space(&self) -> ColorSpace {
        self.space
    }
}

/// Color in internal gamma with premultiplied alpha.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FPixel {
    pub a: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

#[inline(always)]
fn colordifference_ch(x: f64, y: f64, alphas: f64) -> f64 {
    // maximum of channel blended on white, and blended on black
    // premultiplied alpha and backgrounds 0/1 shorten the formula
    let black = x - y;
    let white = black + alphas;
    black * black + white * white
}

#[inline(always)]
fn min_colordifference_ch(x: f64, y: f64, alphas: f64) -> f64 {
    let black = x - y;
    let white = black + alphas;
    cmin(black * black, white * white) * 2.0
}

impl FPixel {
    pub const fn splat(v: f32) -> Self {
        Self { a: v, r: v, g: v, b: v }
    }

    /// Converts 8-bit color to internal gamma and premultiplied alpha.
    #[inline(always)]
    pub fn from_rgba(lut: &ColorLut, px: RGBA) -> Self {
        let a = f32::from(px.a) / 255.0;
        let v = [
            lut.gamma[usize::from(px.r)],
            lut.gamma[usize::from(px.g)],
            lut.gamma[usize::from(px.b)],
        ];
        let [r, g, b] = match lut.space {
            ColorSpace::Rgb => v,
            ColorSpace::Opponent => {
                let [l, c1, c2] = OPPONENT.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2]);
                [l, c1 + CHROMA_OFFSET, c2 + CHROMA_OFFSET]
            }
        };
        Self {
            a,
            r: r * a,
            g: g * a,
            b: b * a,
        }
    }

    pub fn to_rgb(self, gamma: f64, space: ColorSpace) -> RGBA {
        if self.a < 1.0 / 256.0 {
            return RGBA::new(0, 0, 0, 0);
        }
        let (r, g, b) = match space {
            ColorSpace::Rgb => (self.r / self.a, self.g / self.a, self.b / self.a),
            ColorSpace::Opponent => {
                let u = [
                    self.r / self.a,
                    self.g / self.a - CHROMA_OFFSET,
                    self.b / self.a - CHROMA_OFFSET,
                ];
                // colors averaged in this space can fall slightly outside the RGB cube
                let [r, g, b] =
                    OPPONENT_INVERSE.map(|row| (row[0] * u[0] + row[1] * u[1] + row[2] * u[2]).clamp(0.0, 1.0));
                (r, g, b)
            }
        };
        // gamma is narrowed to f32 first, then divided by the f64 constant
        let exponent = (f64::from(gamma as f32) / INTERNAL_GAMMA) as f32;
        // 256, because numbers are in range 1..255.9999… rounded down
        let r = r.powf(exponent) * 256.0;
        let g = g.powf(exponent) * 256.0;
        let b = b.powf(exponent) * 256.0;
        let a = self.a * 256.0;
        let ch = |v: f32| if v >= 255.0 { 255 } else { v as u8 };
        RGBA::new(ch(r), ch(g), ch(b), ch(a))
    }

    /// Perceptual difference, taking the worse of blending on black and on white.
    #[inline(always)]
    pub fn diff(&self, py: &Self) -> f32 {
        let alphas = f64::from(py.a - self.a);
        (colordifference_ch(f64::from(self.r), f64::from(py.r), alphas)
            + colordifference_ch(f64::from(self.g), f64::from(py.g), alphas)
            + colordifference_ch(f64::from(self.b), f64::from(py.b), alphas)) as f32
    }

    /// Least possible difference between colors (it varies with the background they're blended on).
    #[inline(always)]
    pub fn min_diff(&self, py: &Self) -> f32 {
        let alphas = f64::from(py.a - self.a);
        (min_colordifference_ch(f64::from(self.r), f64::from(py.r), alphas)
            + min_colordifference_ch(f64::from(self.g), f64::from(py.g), alphas)
            + min_colordifference_ch(f64::from(self.b), f64::from(py.b), alphas)) as f32
    }

    /// Coordinates in which [`diff`](Self::diff) is a squared Euclidean distance:
    /// the color blended on black (premultiplied `r, g, b`) and the color blended on
    /// white, minus the white background (`r - a, g - a, b - a`). The map is linear, so
    /// weighted means of colors are also the means of their coordinates.
    #[inline(always)]
    pub fn embed(&self) -> [f64; 6] {
        let (a, r, g, b) = (
            f64::from(self.a),
            f64::from(self.r),
            f64::from(self.g),
            f64::from(self.b),
        );
        [r, g, b, r - a, g - a, b - a]
    }

    /// Channel by index: a, r, g, b.
    #[inline(always)]
    pub fn channel(&self, i: usize) -> f32 {
        match i {
            0 => self.a,
            1 => self.r,
            2 => self.g,
            _ => self.b,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PalItem {
    pub color: FPixel,
    pub popularity: f32,
    /// User-supplied color that must not be changed (e.g. in Voronoi iteration).
    pub fixed: bool,
}

/// Palette in internal color space (C `colormap`).
#[derive(Clone, Debug, Default)]
pub(crate) struct PalF {
    pub items: Vec<PalItem>,
    /// Smaller palette remembered by median cut, used to pick vantage points for fast search.
    pub subset: Option<Vec<PalItem>>,
}

impl PalF {
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[inline(always)]
fn posterize_channel(color: u8, bits: u8) -> u8 {
    let color = u32::from(color);
    let bits = u32::from(bits);
    ((color & !((1 << bits) - 1)) | (color >> (8 - bits))) as u8
}

/// Rounds the palette to 8-bit colors, and stores the rounded colors back so that
/// remapping and dithering see exactly what will be written out.
pub(crate) fn set_rounded_palette(map: &mut PalF, gamma: f64, posterize: u8, space: ColorSpace) -> Vec<RGBA> {
    let lut = ColorLut::new(gamma, space);
    map.items
        .iter_mut()
        .map(|item| {
            let mut px = item.color.to_rgb(gamma, space);
            px.r = posterize_channel(px.r, posterize);
            px.g = posterize_channel(px.g, posterize);
            px.b = posterize_channel(px.b, posterize);
            px.a = posterize_channel(px.a, posterize);
            item.color = FPixel::from_rgba(&lut, px);
            if px.a == 0 {
                // libimagequant's signature in fully transparent entries
                px.r = b'L';
                px.g = b'i';
                px.b = b'q';
            }
            px
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn opponent_inverse_is_exact() {
        for (i, row) in OPPONENT.iter().enumerate() {
            for j in 0..3 {
                let v: f32 = (0..3).map(|k| row[k] * OPPONENT_INVERSE[k][j]).sum();
                assert!((v - if i == j { 1.0 } else { 0.0 }).abs() < 1e-6, "{i} {j} {v}");
            }
        }
    }

    #[test]
    fn colors_survive_round_trip() {
        for space in [ColorSpace::Rgb, ColorSpace::Opponent] {
            let lut = ColorLut::new(0.45455, space);
            let mut off_by_one = 0;
            for i in 0..=255u8 {
                for px in [
                    RGBA::new(i, i, i, 255),
                    RGBA::new(i, 255 - i, i / 2, 255),
                    RGBA::new(255, i, 0, 255),
                    RGBA::new(0, i / 3, 255 - i, 200),
                ] {
                    let back = FPixel::from_rgba(&lut, px).to_rgb(0.45455, space);
                    for (a, b) in [(px.r, back.r), (px.g, back.g), (px.b, back.b), (px.a, back.a)] {
                        assert!(a.abs_diff(b) <= 1, "{space:?} {px:?} -> {back:?}");
                        off_by_one += usize::from(a != b);
                    }
                }
            }
            // the opponent matrices are rounded to f32, so a few values land just below an integer
            assert!(off_by_one < 40, "{space:?}: {off_by_one}");
        }
    }

    #[test]
    fn opponent_channels_stay_in_unit_range() {
        let lut = ColorLut::new(0.45455, ColorSpace::Opponent);
        for corner in 0..8u8 {
            let c = |bit: u8| if corner & bit != 0 { 255 } else { 0 };
            let p = FPixel::from_rgba(&lut, RGBA::new(c(1), c(2), c(4), 255));
            for v in [p.r, p.g, p.b] {
                assert!((0.0..=1.0).contains(&v), "{corner}: {p:?}");
            }
        }
    }
}
