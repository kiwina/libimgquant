//! One palette for many images: frames of an animation, sprites of an atlas, tiles of a set.

use std::fmt;

use crate::RGBA;
use crate::attr::{Attributes, Profile};
use crate::error::Error;
use crate::hist::{WEIGHT_UNITS, color_key, count_image_colors, make_histogram, remove_fixed_colors_from_histogram};
use crate::image::Image;
use crate::pal::{ColorLut, FPixel, cmax};
use crate::quant::{QuantizationResult, pngquant_quantize};

/// The gamma that 0 stands for: sRGB.
const SRGB_GAMMA: f64 = 0.45455;

/// A color and how many pixels have it, for [`Histogram::add_colors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HistogramEntry {
    /// The color. Fully transparent colors are all counted as one, whatever their RGB.
    pub color: RGBA,
    /// Number of pixels with this color.
    pub count: u32,
}

/// Colors collected from several images, to compute one palette for all of them.
///
/// Add images with [`add_image`](Self::add_image) (or colors with
/// [`add_colors`](Self::add_colors)), then call [`quantize`](Self::quantize). The returned
/// [`QuantizationResult`] remaps each image to the same palette, so an animated GIF can use
/// one global color table, or a sprite sheet one PLTE chunk:
///
/// ```
/// use libimgquant::{Histogram, RGBA};
///
/// let (width, height) = (32, 32);
/// // Three frames of a moving gradient; only the last has blue in it.
/// let frame = |t: usize| -> Vec<RGBA> {
///     (0..width * height)
///         .map(|i| {
///             let (x, y) = (i % width, i / width);
///             let blue = if t == 2 && x < 8 { 255 } else { 0 };
///             RGBA::new((x * 8 + t * 20) as u8, (y * 8) as u8, blue, 255)
///         })
///         .collect()
/// };
/// let attr = libimgquant::new();
/// let mut frames = (0..3)
///     .map(|t| attr.new_image(frame(t), width, height, 0.0))
///     .collect::<Result<Vec<_>, _>>()?;
///
/// let mut histogram = Histogram::new(&attr);
/// for image in &mut frames {
///     histogram.add_image(&attr, image)?;
/// }
/// let mut result = histogram.quantize(&attr)?;
/// result.set_dithering_level(1.0)?;
///
/// let palette = result.palette_vec();
/// for image in &mut frames {
///     let (frame_palette, indices) = result.remapped(image)?;
///     assert_eq!(frame_palette, palette); // one palette for every frame
///     assert_eq!(indices.len(), width * height);
/// }
/// // the blue of the last frame has palette entries, although 2 of 3 frames have none
/// assert!(palette.iter().any(|c| c.b > 200));
/// # Ok::<(), libimgquant::Error>(())
/// ```
///
/// # How images are weighted
///
/// Each image is counted the way [`Attributes::quantize`] counts it: at speeds that use
/// contrast maps (1-7), pixels in noisy areas count less, and the weight of each color is
/// limited to a tenth of the image's area, so a background color can't take over the
/// palette. The limit applies per image, so a color shared by many frames still gets more
/// weight than a color of one frame.
///
/// # Compared with [`Attributes::quantize`]
///
/// A histogram of one image gives a palette of the same quality as
/// [`Attributes::quantize`], but not the same palette:
///
/// - colors are kept in sorted order rather than in libimagequant 2.4.1's hash table order,
///   which changes which of two equal choices median cut makes;
/// - weights are summed exactly, also with [`Profile::Compat`] (which otherwise reproduces
///   2.4.1's `f32` sums), and the histogram entry limit of the speed setting is a plain limit
///   on the number of colors;
/// - remapping keeps the palette as it is: [`Attributes::quantize`] lets remapping move the
///   palette towards the image's colors and, with [`Profile::Improved`], reorder it for PNG
///   compression; neither would give every image the same palette.
///
/// Colors are stored exactly until [`quantize`](Self::quantize), which posterizes them only
/// if there are more than the speed setting allows. The result is the same for any number of
/// threads.
#[derive(Clone)]
pub struct Histogram {
    /// Colors as [`color_key`]s with their weights in units of 1/[`WEIGHT_UNITS`] pixel,
    /// sorted by key.
    colors: Vec<(u32, u64)>,
    fixed: Vec<RGBA>,
    /// The gamma of everything added so far (0 already replaced by sRGB's).
    gamma: Option<f64>,
    profile: Profile,
}

impl fmt::Debug for Histogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Histogram")
            .field("colors", &self.colors.len())
            .field("fixed_colors", &self.fixed)
            .field("gamma", &self.gamma)
            .field("profile", &self.profile)
            .finish()
    }
}

/// `gamma` with 0 replaced by sRGB's, or an error outside 0-1 (as for images).
fn normalize_gamma(gamma: f64) -> Result<f64, Error> {
    match gamma {
        0.0 => Ok(SRGB_GAMMA),
        g if g > 0.0 && g <= 1.0 => Ok(g),
        _ => Err(Error::ValueOutOfRange),
    }
}

/// Merges two lists sorted by color, adding the weights of equal colors.
fn merge(a: &[(u32, u64)], b: &[(u32, u64)]) -> Vec<(u32, u64)> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        match x.0.cmp(&y.0) {
            std::cmp::Ordering::Less => {
                out.push(x);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(y);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push((x.0, x.1.saturating_add(y.1)));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Sorts by color and adds up the weights of equal colors. Integer sums are exact, so the
/// order of equal colors after the (unstable) sort doesn't matter.
fn sort_and_sum(mut colors: Vec<(u32, u64)>) -> Vec<(u32, u64)> {
    colors.sort_unstable_by_key(|&(color, _)| color);
    colors.dedup_by(|next, kept| {
        if next.0 == kept.0 {
            kept.1 = kept.1.saturating_add(next.1);
            true
        } else {
            false
        }
    });
    colors
}

impl Histogram {
    /// Creates an empty histogram for images quantized with `attr`'s [`Profile`].
    #[must_use]
    pub fn new(attr: &Attributes) -> Self {
        Self {
            colors: Vec::new(),
            fixed: Vec::new(),
            gamma: None,
            profile: attr.profile,
        }
    }

    fn check(&self, attr: &Attributes, gamma: f64) -> Result<(), Error> {
        if attr.profile != self.profile {
            attr.log_error("the histogram was created with another profile");
            return Err(Error::ValueOutOfRange);
        }
        if self.gamma.is_some_and(|g| g != gamma) {
            attr.log_error("all images of a histogram must have the same gamma");
            return Err(Error::ValueOutOfRange);
        }
        Ok(())
    }

    /// Adds the colors of `image`, weighted as [`Attributes::quantize`] weights them (see
    /// [How images are weighted](Self#how-images-are-weighted)), and its fixed colors (see
    /// [`Image::add_fixed_color`]).
    ///
    /// `attr` sets the speed, which decides whether contrast maps are used. The image keeps
    /// the edge map it computes here for dithering when it is remapped later.
    ///
    /// # Errors
    ///
    /// - [`Error::ValueOutOfRange`] if the image's gamma differs from the gamma of the colors
    ///   added before (gamma 0 and 0.45455 are the same), or `attr` has another [`Profile`]
    ///   than the histogram was created with.
    /// - [`Error::BufferTooSmall`] if there would be more than 256 fixed colors.
    pub fn add_image(&mut self, attr: &Attributes, image: &mut Image<'_>) -> Result<(), Error> {
        self.check(attr, image.gamma)?;
        let new_fixed: Vec<RGBA> = image
            .fixed_rgba
            .iter()
            .copied()
            .filter(|c| !self.fixed.contains(c))
            .collect();
        if self.fixed.len() + new_fixed.len() > 256 {
            return Err(Error::BufferTooSmall);
        }
        image.ensure_color_space(self.profile.color_space());
        let counts = count_image_colors(image, attr);
        attr.verbose_print(&format!("  added image to histogram...{} colors", counts.len()));
        self.colors = merge(&self.colors, &counts);
        for c in new_fixed {
            if !self.fixed.contains(&c) {
                self.fixed.push(c);
            }
        }
        self.gamma = Some(image.gamma);
        Ok(())
    }

    /// Adds colors with their pixel counts, for colors that come from elsewhere than an
    /// [`Image`] (another histogram, a video decoder). Counts are used as they are, without
    /// contrast weighting or the tenth-of-the-area limit.
    ///
    /// `gamma` is the gamma of the colors, as for [`Attributes::new_image`]; 0 means sRGB.
    ///
    /// ```
    /// use libimgquant::{Histogram, HistogramEntry, RGBA};
    ///
    /// let attr = libimgquant::new();
    /// let mut histogram = Histogram::new(&attr);
    /// let entries: Vec<HistogramEntry> = (0..=255u8)
    ///     .map(|v| HistogramEntry { color: RGBA::new(v, 255 - v, 128, 255), count: 10 })
    ///     .collect();
    /// histogram.add_colors(&entries, 0.0)?;
    ///
    /// let mut few = attr.clone();
    /// few.set_max_colors(16)?;
    /// let mut result = histogram.quantize(&few)?;
    /// assert_eq!(result.palette().len(), 16);
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] if `gamma` is outside 0-1 or differs from the gamma of the
    /// colors added before.
    pub fn add_colors(&mut self, entries: &[HistogramEntry], gamma: f64) -> Result<(), Error> {
        let gamma = normalize_gamma(gamma)?;
        if self.gamma.is_some_and(|g| g != gamma) {
            return Err(Error::ValueOutOfRange);
        }
        let counts: Vec<(u32, u64)> = entries
            .iter()
            .filter(|e| e.count > 0)
            .map(|e| (color_key(e.color, 0), u64::from(e.count) * WEIGHT_UNITS))
            .collect();
        self.colors = merge(&self.colors, &sort_and_sum(counts));
        self.gamma = Some(gamma);
        Ok(())
    }

    /// Reserves a color that will be in the palette, like [`Image::add_fixed_color`]. Fixed
    /// colors count towards [`Attributes::set_max_colors`]; adding a color twice has no
    /// effect.
    ///
    /// ```
    /// use libimgquant::{Histogram, RGBA};
    ///
    /// let pixels: Vec<RGBA> = (0..64u8).map(|i| RGBA::new(i * 4, 100, 50, 255)).collect();
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image(pixels, 8, 8, 0.0)?;
    /// let mut histogram = Histogram::new(&attr);
    /// histogram.add_image(&attr, &mut image)?;
    /// // the transparent color of a GIF, for example
    /// histogram.add_fixed_color(RGBA::new(0, 0, 0, 0), 0.0)?;
    /// let palette = histogram.quantize(&attr)?.palette_vec();
    /// assert!(palette.iter().any(|c| c.a == 0));
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::ValueOutOfRange`] if `gamma` is outside 0-1 or differs from the gamma of
    ///   the colors added before.
    /// - [`Error::BufferTooSmall`] if there are already 256 fixed colors.
    pub fn add_fixed_color(&mut self, color: RGBA, gamma: f64) -> Result<(), Error> {
        let gamma = normalize_gamma(gamma)?;
        if self.gamma.is_some_and(|g| g != gamma) {
            return Err(Error::ValueOutOfRange);
        }
        if !self.fixed.contains(&color) {
            if self.fixed.len() >= 256 {
                return Err(Error::BufferTooSmall);
            }
            self.fixed.push(color);
        }
        self.gamma = Some(gamma);
        Ok(())
    }

    /// Computes one palette for everything added. Remap each image with the returned
    /// [`QuantizationResult`]; every remapping uses the same palette, which
    /// [`palette`](QuantizationResult::palette) returns before and after remapping.
    ///
    /// The histogram is kept, so more colors can be added and it can be quantized again.
    ///
    /// # Errors
    ///
    /// - [`Error::QualityTooLow`] if the palette can't reach the minimum quality set with
    ///   [`Attributes::set_quality`].
    /// - [`Error::ValueOutOfRange`] if nothing was added, or `attr` has another [`Profile`]
    ///   than the histogram was created with.
    pub fn quantize(&mut self, attr: &Attributes) -> Result<QuantizationResult, Error> {
        let gamma = self.gamma.unwrap_or(SRGB_GAMMA);
        self.check(attr, gamma)?;
        let lut = ColorLut::new(gamma, self.profile.color_space());

        // As for single images: posterize only when there are too many colors.
        let mut ignorebits = u32::from(cmax(attr.min_posterization_output, attr.min_posterization_input));
        let colors = loop {
            let colors = if ignorebits == 0 {
                self.colors.clone()
            } else {
                let posterized = self.colors.iter().map(|&(key, w)| {
                    let [r, g, b, a] = key.to_le_bytes();
                    (color_key(RGBA::new(r, g, b, a), ignorebits), w)
                });
                sort_and_sum(posterized.collect())
            };
            if colors.len() <= attr.max_histogram_entries as usize {
                break colors;
            }
            if ignorebits >= 7 {
                return Err(Error::OutOfMemory);
            }
            ignorebits += 1;
            attr.verbose_print(&format!(
                "  too many colors! Scaling colors to improve clustering... {ignorebits}"
            ));
        };

        let len = colors.len();
        let weights = colors
            .into_iter()
            .map(|(key, w)| (key, (w as f64 / WEIGHT_UNITS as f64) as f32));
        // weights were limited per image already
        let mut hist = make_histogram(weights, len, &lut, f32::INFINITY, ignorebits);
        attr.verbose_print(&format!("  made histogram...{len} colors found"));

        let fixed: Vec<FPixel> = self.fixed.iter().map(|&c| FPixel::from_rgba(&lut, c)).collect();
        remove_fixed_colors_from_histogram(&mut hist, &fixed, attr.target_mse as f32);
        let mut result = pngquant_quantize(&mut hist, attr, &fixed, gamma)?;
        result.set_shared_palette();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adds_equal_colors() {
        let a = [(1, 10), (5, 1), (9, 2)];
        let b = [(0, 3), (5, 4), (10, 1)];
        assert_eq!(merge(&a, &b), [(0, 3), (1, 10), (5, 5), (9, 2), (10, 1)]);
        assert_eq!(merge(&[], &b), b);
        assert_eq!(
            sort_and_sum(vec![(4, 1), (2, 2), (4, 3), (2, u64::MAX)]),
            [(2, u64::MAX), (4, 4)]
        );
    }

    #[test]
    fn transparent_colors_are_one_color() {
        let attr = crate::new();
        let mut h = Histogram::new(&attr);
        let entries = [
            HistogramEntry {
                color: RGBA::new(1, 2, 3, 0),
                count: 2,
            },
            HistogramEntry {
                color: RGBA::new(9, 9, 9, 0),
                count: 3,
            },
        ];
        h.add_colors(&entries, 0.0).unwrap();
        assert_eq!(h.colors, [(0, 5 * WEIGHT_UNITS)]);
    }

    #[test]
    fn image_weights_are_limited_to_a_tenth_of_the_area() {
        // one color everywhere but one pixel, no contrast maps (speed 10)
        let mut attr = crate::new();
        attr.set_speed(10).unwrap();
        let mut pixels = vec![RGBA::new(10, 20, 30, 255); 100];
        pixels[0] = RGBA::new(200, 0, 0, 255);
        let mut image = attr.new_image(pixels, 10, 10, 0.0).unwrap();
        let mut h = Histogram::new(&attr);
        h.add_image(&attr, &mut image).unwrap();
        h.add_image(&attr, &mut image).unwrap();
        let weight = |c: RGBA| h.colors.iter().find(|e| e.0 == color_key(c, 0)).unwrap().1;
        // 10 pixels (a tenth of 100) per image, twice
        assert_eq!(weight(RGBA::new(10, 20, 30, 255)), 2 * 10 * WEIGHT_UNITS);
        assert_eq!(weight(RGBA::new(200, 0, 0, 255)), 2 * WEIGHT_UNITS);
    }
}
