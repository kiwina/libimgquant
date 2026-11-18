//! Palette search and the quantization result.

use std::cmp::Ordering;
use std::fmt;

use crate::RGBA;
use crate::attr::{Attributes, Profile, mse_to_quality};
use crate::dithermap::update_dither_map;
use crate::error::Error;
use crate::hist::HistogramInternal;
use crate::image::Image;
use crate::kmeans::{hartigan_refine, viter_do_iteration};
use crate::mediancut::mediancut;
use crate::pal::{FPixel, MAX_DIFF, PalF, PalItem, cmax, cmin, set_rounded_palette};
use crate::remap::{remap_to_palette, remap_to_palette_floyd};
use crate::reorder::reorder_palette;

/// Result of [`Attributes::quantize`]: a palette that can be used to remap images.
///
/// One result can remap several images. Each remapping can reorder the palette and fine-tune
/// its colors for its image (see [`palette`](Self::palette)); for frames that must share
/// exactly one palette, such as a GIF's global color table, quantize a
/// [`Histogram`](crate::Histogram) of all frames.
///
/// ```
/// use libimgquant::RGBA;
///
/// let frame = |shift: u8| -> Vec<RGBA> {
///     (0..256u32).map(|i| RGBA::new(i as u8, shift.wrapping_add(i as u8 / 2), 60, 255)).collect()
/// };
/// let attr = libimgquant::new();
/// let mut first = attr.new_image(frame(0), 16, 16, 0.0)?;
/// let mut second = attr.new_image(frame(40), 16, 16, 0.0)?;
///
/// let mut result = attr.quantize(&mut first)?;
/// result.set_dithering_level(1.0)?;
/// let mut out = vec![0u8; 16 * 16];
/// result.remap_into(&mut first, &mut out)?;
/// result.remap_into(&mut second, &mut out)?;
/// let palette = result.palette(); // the palette of the last remapping
/// assert!(out.iter().all(|&i| usize::from(i) < palette.len()));
/// # Ok::<(), libimgquant::Error>(())
/// ```
#[derive(Clone)]
pub struct QuantizationResult {
    palette: PalF,
    /// Rounded palette, computed on first request.
    int_palette: Option<Vec<RGBA>>,
    remapping: Option<Remapped>,
    dither_level: f32,
    gamma: f64,
    /// Negative when unknown.
    palette_error: f64,
    min_posterization_output: u8,
    use_dither_map: bool,
    fast_palette: bool,
    last_index_transparent: bool,
    reorder_palette: bool,
    profile: Profile,
    /// Every remapping uses exactly [`palette`](QuantizationResult::palette): no per-image
    /// k-means update and no reordering, so frames can share one color table.
    shared_palette: bool,
}

impl fmt::Debug for QuantizationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuantizationResult")
            .field("colors", &self.palette.len())
            .field("output_gamma", &self.gamma)
            .field("dithering_level", &self.dither_level)
            .field("profile", &self.profile)
            .field("quantization_error", &self.quantization_error())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Remapped {
    int_palette: Vec<RGBA>,
    palette_error: f64,
}

/// Sorts by descending popularity. The original's comparator never reported equality,
/// which with a merge sort puts equally popular colors in reverse order; this does the
/// same deterministically on every platform.
fn sort_by_popularity(items: &mut [PalItem]) {
    items.reverse();
    items.sort_by(|a, b| b.popularity.partial_cmp(&a.popularity).unwrap_or(Ordering::Equal));
}

fn sort_palette(map: &mut PalF, attr: &Attributes) {
    let items = &mut map.items;
    let len = items.len();

    // Workaround for blu-ray subtitles: transparent color at the last index.
    if attr.last_index_transparent {
        if let Some(i) = items.iter().position(|p| p.color.a < 1.0 / 256.0) {
            items.swap(len - 1, i);
            // colors sorted by popularity make pngs slightly more compressible
            sort_by_popularity(&mut items[..len - 1]);
            return;
        }
    }

    // move transparent colors to the beginning to shrink trns chunk
    let mut num_transparent = 0;
    for i in 0..len {
        if items[i].color.a < 255.0 / 256.0 {
            // current transparent color is swapped with earlier opaque one
            items.swap(num_transparent, i);
            num_transparent += 1;
        }
    }

    attr.verbose_print(&format!(
        "  eliminated opaque tRNS-chunk entries...{num_transparent} entr{} transparent",
        if num_transparent == 1 { "y" } else { "ies" }
    ));

    // colors sorted by popularity make pngs slightly more compressible.
    // opaque and transparent are sorted separately
    let (transparent, opaque) = items.split_at_mut(num_transparent);
    sort_by_popularity(transparent);
    sort_by_popularity(opaque);

    if len > 16 {
        // slightly improves compression
        items.swap(7, 1);
        items.swap(8, 2);
        items.swap(9, 3);
    }
}

fn add_fixed_colors_to_palette(palette: Option<PalF>, max_colors: usize, fixed_colors: &[FPixel]) -> Option<PalF> {
    if fixed_colors.is_empty() {
        return palette;
    }

    let mut items = Vec::with_capacity(cmin(
        max_colors,
        palette.as_ref().map_or(0, PalF::len) + fixed_colors.len(),
    ));
    if let Some(palette) = &palette {
        if fixed_colors.len() < max_colors {
            let palette_max = cmin(palette.len(), max_colors - fixed_colors.len());
            items.extend_from_slice(&palette.items[..palette_max]);
        }
    }
    items.extend(fixed_colors.iter().take(max_colors).map(|&color| PalItem {
        color,
        popularity: 0.0,
        fixed: true,
    }));
    Some(PalF { items, subset: None })
}

/// Repeats median cut with different histogram weights to find the palette with minimum error.
///
/// `feedback_loop_trials` controls how long the search will take. Returns the palette and its error (negative if not measured).
fn find_best_palette(
    hist: &mut HistogramInternal,
    attr: &Attributes,
    max_mse: f64,
    fixed_colors: &[FPixel],
) -> Result<(PalF, f64), Error> {
    let mut max_colors = attr.max_colors as usize;

    // if output is posterized it doesn't make sense to aim for perfect colors, so increase target_mse
    // at this point actual gamma is not set, so very conservative posterization estimate is used
    let posterized = f64::from(1u32 << attr.min_posterization_output) / 1024.0;
    let target_mse = cmin(max_mse, cmax(attr.target_mse, posterized * posterized));
    let mut feedback_loop_trials = attr.feedback_loop_trials as i32;
    let mut best: Option<PalF> = None;
    let mut least_error = MAX_DIFF;
    let mut target_mse_overshoot = if feedback_loop_trials > 0 { 1.05 } else { 1.0 };
    let percent = f64::from(if feedback_loop_trials > 0 {
        feedback_loop_trials
    } else {
        1
    }) / 100.0;

    loop {
        let newmap = if !hist.items.is_empty() && fixed_colors.len() < max_colors {
            Some(mediancut(
                hist,
                attr.min_opaque_val,
                max_colors - fixed_colors.len(),
                target_mse * target_mse_overshoot,
                cmax(cmax(90.0 / 65536.0, target_mse), least_error) * 1.2,
                attr.profile,
            ))
        } else {
            feedback_loop_trials = 0;
            None
        };
        let Some(mut newmap) = add_fixed_colors_to_palette(newmap, max_colors, fixed_colors) else {
            return Err(Error::ValueOutOfRange);
        };

        if feedback_loop_trials <= 0 {
            return Ok((newmap, -1.0));
        }

        // after palette has been created, total error (MSE) is calculated to keep the best palette
        // at the same time Voronoi iteration is done to improve the palette
        // and histogram weights are adjusted based on remapping error to give more weight to poorly matched colors
        let first_run_of_target_mse = best.is_none() && target_mse > 0.0;
        let total_error = viter_do_iteration(
            hist,
            &mut newmap,
            attr.min_opaque_val,
            !first_run_of_target_mse,
            best.is_none() || attr.fast_palette,
            attr.profile,
        );

        // goal is to increase quality or to reduce number of colors used if quality is good enough
        if best.is_none() || total_error < least_error || (total_error <= target_mse && newmap.len() < max_colors) {
            if total_error < target_mse && total_error > 0.0 {
                // Voronoi iteration improves quality above what median cut aims for.
                // This compensates for it, making median cut aim for worse.
                target_mse_overshoot = cmin(target_mse_overshoot * 1.25, target_mse / total_error);
            }

            least_error = total_error;

            // if number of colors could be reduced, try to keep it that way
            // but allow extra color as a bit of wiggle room in case quality can be improved too
            max_colors = cmin(newmap.len() + 1, max_colors);
            best = Some(newmap);

            feedback_loop_trials -= 1; // asymptotic improvement could make it go on forever
        } else {
            for item in &mut hist.items {
                item.adjusted_weight = (f64::from(item.perceptual_weight + item.adjusted_weight) / 2.0) as f32;
            }

            target_mse_overshoot = 1.0;
            feedback_loop_trials -= 6;
            // if error is really bad, it's unlikely to improve, so end sooner
            if total_error > least_error * 4.0 {
                feedback_loop_trials -= 3;
            }
        }

        attr.verbose_print(&format!(
            "  selecting colors...{}%",
            100 - cmax(0, (f64::from(feedback_loop_trials) / percent) as i32)
        ));
        if feedback_loop_trials <= 0 {
            break;
        }
    }

    Ok((best.expect("at least one palette was kept"), least_error))
}

/// Searches the palette for `hist` (from which colors close to `fixed_colors` were removed),
/// for colors in `gamma`.
pub(crate) fn pngquant_quantize(
    hist: &mut HistogramInternal,
    attr: &Attributes,
    fixed_colors: &[FPixel],
    gamma: f64,
) -> Result<QuantizationResult, Error> {
    // no point having perfect match with imperfect colors (ignorebits > 0)
    let fast_palette = attr.fast_palette || hist.ignorebits > 0;
    let few_input_colors = hist.items.len() + fixed_colors.len() <= attr.max_colors as usize;

    let mut palette_error;
    let mut palette;
    // If image has few colors to begin with (and no quality degradation is required)
    // then it's possible to skip quantization entirely
    if few_input_colors && attr.target_mse == 0.0 {
        let items = hist
            .items
            .iter()
            .map(|h| PalItem {
                color: h.color,
                popularity: h.perceptual_weight,
                fixed: false,
            })
            .collect();
        palette = add_fixed_colors_to_palette(
            Some(PalF { items, subset: None }),
            attr.max_colors as usize,
            fixed_colors,
        )
        .expect("palette given");
        if palette.items.is_empty() {
            return Err(Error::ValueOutOfRange);
        }
        palette_error = 0.0;
    } else {
        // when degrading image that's already paletted, require much higher improvement,
        // since pal2pal often looks bad and there's little gain
        let max_mse = attr.max_mse * if few_input_colors { 0.33 } else { 1.0 };
        (palette, palette_error) = find_best_palette(hist, attr, max_mse, fixed_colors)?;

        // Voronoi iteration approaches local minimum for the palette
        let iteration_limit = attr.voronoi_iteration_limit;
        let mut iterations = attr.voronoi_iterations;

        if iterations == 0 && palette_error < 0.0 && max_mse < MAX_DIFF {
            iterations = 1; // otherwise total error is never calculated and MSE limit won't work
        }

        if iterations > 0 {
            // likely_colormap_index (used and set in viter_do_iteration) can't point to index outside colormap
            if palette.len() < 256 {
                for item in &mut hist.items {
                    if usize::from(item.likely_colormap_index) >= palette.len() {
                        item.likely_colormap_index = 0; // actual value doesn't matter, as the guess is out of date anyway
                    }
                }
            }

            attr.verbose_print("  moving colormap towards local minimum");

            let mut previous_palette_error = MAX_DIFF;
            let mut i = 0;
            while i < iterations {
                palette_error = viter_do_iteration(
                    hist,
                    &mut palette,
                    attr.min_opaque_val,
                    false,
                    i == 0 || attr.fast_palette,
                    attr.profile,
                );

                if (previous_palette_error - palette_error).abs() < iteration_limit {
                    break;
                }

                if palette_error > max_mse * 1.5 {
                    // probably hopeless
                    if palette_error > max_mse * 3.0 {
                        break; // definitely hopeless
                    }
                    i += 1;
                }

                previous_palette_error = palette_error;
                i += 1;
            }
        }

        // the original's penalty for translucent colors under min_opacity isn't a metric, so Hartigan's rule doesn't apply
        if attr.hartigan_passes > 0 && attr.min_opaque_val >= 1.0 && palette.len() > 1 {
            let before = palette.clone();
            if hartigan_refine(hist, &mut palette, attr.hartigan_passes) > 0 {
                // refreshes the error and likely indices for the refined palette
                let refined_error = viter_do_iteration(
                    hist,
                    &mut palette,
                    attr.min_opaque_val,
                    false,
                    attr.fast_palette,
                    attr.profile,
                );
                if palette_error < 0.0 || refined_error <= palette_error {
                    palette_error = refined_error;
                } else {
                    palette = before;
                }
            }
        }

        if palette_error > max_mse {
            attr.verbose_print(&format!(
                "  image degradation MSE={:.3} (Q={}) exceeded limit of {:.3} ({})",
                palette_error * 65536.0 / 6.0,
                mse_to_quality(palette_error),
                max_mse * 65536.0 / 6.0,
                mse_to_quality(max_mse)
            ));
            return Err(Error::QualityTooLow);
        }
    }

    sort_palette(&mut palette, attr);

    Ok(QuantizationResult {
        palette,
        int_palette: None,
        remapping: None,
        dither_level: 0.0,
        gamma,
        palette_error,
        min_posterization_output: attr.min_posterization_output,
        use_dither_map: attr.use_dither_map,
        fast_palette,
        last_index_transparent: attr.last_index_transparent,
        reorder_palette: attr.reorder_palette,
        profile: attr.profile,
        shared_palette: false,
    })
}

impl QuantizationResult {
    /// Makes every remapping use the same palette (see [`crate::Histogram`]).
    pub(crate) fn set_shared_palette(&mut self) {
        self.shared_palette = true;
    }

    /// Sets the dithering level for subsequent remappings: 0 (none) to 1 (full).
    /// The default is 0, as in libimagequant 2.4.1.
    ///
    /// Dithering is Floyd-Steinberg error diffusion; at speeds 1-5 a dither map reduces it
    /// near edges, where it would add noise.
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] outside 0-1.
    pub fn set_dithering_level(&mut self, value: f32) -> Result<(), Error> {
        // the original validated the previous value instead of the new one
        if !(0.0..=1.0).contains(&value) {
            return Err(Error::ValueOutOfRange);
        }
        self.remapping = None;
        self.dither_level = value;
        Ok(())
    }

    /// The dithering level used for remapping.
    #[must_use]
    pub fn dithering_level(&self) -> f32 {
        self.dither_level
    }

    /// Sets gamma of the output palette, between 0 and 1 (exclusive). Defaults to the input image's gamma.
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] outside the open range 0-1.
    pub fn set_output_gamma(&mut self, value: f64) -> Result<(), Error> {
        if !(value > 0.0 && value < 1.0) {
            return Err(Error::ValueOutOfRange);
        }
        self.remapping = None;
        // the original kept returning a palette rounded with the previous gamma
        self.int_palette = None;
        self.gamma = value;
        Ok(())
    }

    /// Gamma of the output palette.
    #[must_use]
    pub fn output_gamma(&self) -> f64 {
        self.gamma
    }

    /// Mean square error of the palette, scaled like pngquant's `--verbose` output.
    /// `None` if it has not been measured (it is after remapping).
    #[must_use]
    pub fn quantization_error(&self) -> Option<f64> {
        self.known_error().map(|e| e * 65536.0 / 6.0)
    }

    /// Quality 0-100 matching [`Attributes::set_quality`]. `None` if it has not been measured (it is after remapping).
    ///
    /// ```
    /// let pixels: Vec<_> = (0..1024u32).map(|i| libimgquant::RGBA::new(i as u8, (i / 4) as u8, 7, 255)).collect();
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image(pixels, 32, 32, 0.0)?;
    /// let mut result = attr.quantize(&mut image)?;
    /// result.remapped(&mut image)?;
    /// let quality = result.quantization_quality().expect("measured by remapping");
    /// assert!(quality <= 100);
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    #[must_use]
    pub fn quantization_quality(&self) -> Option<u8> {
        self.known_error().map(|e| mse_to_quality(e) as u8)
    }

    fn known_error(&self) -> Option<f64> {
        if self.palette_error >= 0.0 {
            return Some(self.palette_error);
        }
        self.remapping.as_ref().map(|r| r.palette_error).filter(|&e| e >= 0.0)
    }

    /// The final palette. After remapping, this is the palette the remapped pixels refer to.
    pub fn palette(&mut self) -> &[RGBA] {
        if let Some(r) = &self.remapping {
            if !r.int_palette.is_empty() {
                return &r.int_palette;
            }
        }
        if self.int_palette.is_none() {
            self.int_palette = Some(set_rounded_palette(
                &mut self.palette,
                self.gamma,
                self.min_posterization_output,
                self.profile.color_space(),
            ));
        }
        self.int_palette.as_deref().unwrap_or_default()
    }

    /// Same as [`palette`](Self::palette), as an owned `Vec`.
    pub fn palette_vec(&mut self) -> Vec<RGBA> {
        self.palette().to_vec()
    }

    /// Remaps the image into a new buffer, and returns the palette and palette indices (row by row).
    ///
    /// # Errors
    ///
    /// Currently never fails, since the buffer is allocated to fit; the `Result` matches
    /// [`remap_into`](Self::remap_into).
    pub fn remapped(&mut self, image: &mut Image<'_>) -> Result<(Vec<RGBA>, Vec<u8>), Error> {
        let mut buf = vec![0u8; image.width() * image.height()];
        self.remap_into(image, &mut buf)?;
        Ok((self.palette_vec(), buf))
    }

    /// Remaps the image into `output`, which must hold at least `width * height` bytes (no padding between rows).
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooSmall`] if `output` is too short.
    pub fn remap_into(&mut self, image: &mut Image<'_>, output: &mut [u8]) -> Result<(), Error> {
        let width = image.width();
        let required = width * image.height();
        if output.len() < required {
            return Err(Error::BufferTooSmall);
        }
        let mut rows: Vec<&mut [u8]> = output[..required].chunks_exact_mut(width).collect();
        self.remap_rows(image, &mut rows)
    }

    /// Remaps the image into separate rows. There must be at least `height` rows of at least `width` bytes.
    ///
    /// Use it to write into a buffer with padding between rows:
    ///
    /// ```
    /// let pixels = vec![libimgquant::RGBA::new(1, 2, 3, 255); 10 * 4];
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image(pixels, 10, 4, 0.0)?;
    /// let mut result = attr.quantize(&mut image)?;
    ///
    /// let stride = 16;
    /// let mut buffer = vec![0xffu8; stride * 4];
    /// let mut rows: Vec<&mut [u8]> = buffer.chunks_exact_mut(stride).collect();
    /// result.remap_rows(&mut image, &mut rows)?;
    /// assert_eq!(buffer[0], 0); // the only color
    /// assert_eq!(buffer[10], 0xff); // padding is left alone
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooSmall`] if there are fewer than `height` rows or a row is shorter
    /// than `width`.
    pub fn remap_rows(&mut self, image: &mut Image<'_>, rows: &mut [&mut [u8]]) -> Result<(), Error> {
        let (width, height) = (image.width(), image.height());
        if rows.len() < height || rows[..height].iter().any(|r| r.len() < width) {
            return Err(Error::BufferTooSmall);
        }
        let mut rows: Vec<&mut [u8]> = rows[..height].iter_mut().map(|r| &mut r[..width]).collect();
        let rows = &mut rows[..];

        self.remapping = None;
        image.ensure_color_space(self.profile.color_space());
        let mut palette = self.palette.clone();
        let mut palette_error = self.palette_error;

        if image.edges.is_none() && image.dither_map.is_none() && self.use_dither_map {
            image.contrast_maps();
        }

        // Map the colors in the image to their closest match in the new colormap.
        let mut remapping_error = palette_error as f32;
        let mut int_palette;
        if self.dither_level == 0.0 {
            int_palette = set_rounded_palette(
                &mut palette,
                self.gamma,
                self.min_posterization_output,
                self.profile.color_space(),
            );
            remapping_error = remap_to_palette(image, rows, &mut palette, self.fast_palette, self.profile);
        } else {
            let generate_dither_map = self.use_dither_map && image.edges.is_some() && image.dither_map.is_none();
            if generate_dither_map {
                // If dithering (with dither map) is required, this image is used to find areas that require dithering
                // A shared palette must not be moved towards this image's colors.
                let mut scratch;
                let map = if self.shared_palette {
                    scratch = palette.clone();
                    &mut scratch
                } else {
                    &mut palette
                };
                remapping_error = remap_to_palette(image, rows, map, self.fast_palette, self.profile);
                update_dither_map(rows, image);
            }

            // remapping above was the last chance to do Voronoi iteration, hence the final palette is set after remapping
            int_palette = set_rounded_palette(
                &mut palette,
                self.gamma,
                self.min_posterization_output,
                self.profile.color_space(),
            );

            let max_dither_error = cmax(f64::from(remapping_error) * 2.4, f64::from(16.0f32 / 256.0)) as f32;
            remap_to_palette_floyd(
                image,
                rows,
                &palette,
                max_dither_error,
                self.use_dither_map,
                generate_dither_map,
                self.dither_level,
                self.profile,
            );
        }

        // remapping error from dithered image is absurd, so always non-dithered value is used.
        // palette_error includes some perceptual weighting from histogram which is closer correlated with dssim
        // so that should be used when possible.
        if palette_error < 0.0 {
            palette_error = f64::from(remapping_error);
        }

        // a transparent entry put last (set_last_index_transparent) must stay last
        let transparent_last = self.last_index_transparent && int_palette.last().is_some_and(|c| c.a == 0);
        if self.profile == Profile::Improved && !transparent_last && !self.shared_palette {
            reorder_palette(&mut int_palette, rows, self.reorder_palette);
        }

        self.remapping = Some(Remapped {
            int_palette,
            palette_error,
        });
        Ok(())
    }
}
