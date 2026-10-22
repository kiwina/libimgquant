//! Quantization settings.

use std::fmt;
use std::sync::Arc;

use crate::RGBA;
use crate::error::Error;
use crate::hist::get_histogram;
use crate::image::{Image, PixelData, Source};
use crate::pal::{ColorSpace, MAX_DIFF, cmax, cmin};
use crate::quant::{QuantizationResult, pngquant_quantize};

type LogCallback = Arc<dyn Fn(&Attributes, &str) + Send + Sync>;

/// Which set of algorithms to use, selected with [`Attributes::set_profile`].
///
/// ```
/// use libimgquant::Profile;
///
/// let mut attr = libimgquant::new();
/// assert_eq!(attr.profile(), Profile::Improved);
/// // Reproduce libimagequant 2.4.1's output.
/// attr.set_profile(Profile::Compat);
/// assert_eq!(attr.profile(), Profile::Compat);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Profile {
    /// The algorithms of libimagequant 2.4.1, reproduced step by step (apart from the
    /// bug fixes listed in the README). Output is frozen by golden tests.
    Compat,
    /// The default: improvements from color quantization research. An exact nearest-color
    /// search, variance-based median cut splits, Hartigan refinement, an OKLab-shaped color
    /// difference, and a palette order that compresses better. Faster, better looking and
    /// smaller output than [`Compat`](Self::Compat).
    #[default]
    Improved,
}

impl Profile {
    pub(crate) fn color_space(self) -> ColorSpace {
        match self {
            Self::Compat => ColorSpace::Rgb,
            Self::Improved => ColorSpace::Opponent,
        }
    }
}
type LogFlushCallback = Arc<dyn Fn(&Attributes) + Send + Sync>;

/// Quantization settings. Start with [`Attributes::new`], or [`crate::new`].
///
/// The attributes create images ([`new_image`](Self::new_image) and its variants) and
/// quantize them ([`quantize`](Self::quantize)). Changing a setting does not affect images or
/// results created before.
///
/// ```
/// let mut attr = libimgquant::Attributes::new();
/// attr.set_max_colors(64)?;
/// attr.set_speed(1)?;
/// attr.set_quality(50, 95)?;
/// attr.set_min_posterization(1)?; // ignore the lowest bit, e.g. for RGB565 displays
/// assert_eq!(attr.quality(), (50, 95));
///
/// // Settings can be copied.
/// let fast = {
///     let mut a = attr.clone();
///     a.set_speed(10)?;
///     a
/// };
/// assert_eq!((fast.max_colors(), fast.speed()), (64, 10));
/// # Ok::<(), libimgquant::Error>(())
/// ```
#[derive(Clone)]
pub struct Attributes {
    pub(crate) target_mse: f64,
    pub(crate) max_mse: f64,
    pub(crate) voronoi_iteration_limit: f64,
    pub(crate) min_opaque_val: f32,
    pub(crate) max_colors: u32,
    pub(crate) max_histogram_entries: u32,
    /// User setting.
    pub(crate) min_posterization_output: u8,
    /// Speed setting.
    pub(crate) min_posterization_input: u8,
    pub(crate) voronoi_iterations: u32,
    /// Passes of Hartigan refinement after the Voronoi iterations (Improved profile).
    pub(crate) hartigan_passes: u32,
    /// Order the remapped palette for compression (Improved profile, not at the fastest speeds).
    pub(crate) reorder_palette: bool,
    pub(crate) feedback_loop_trials: u32,
    pub(crate) last_index_transparent: bool,
    pub(crate) use_contrast_maps: bool,
    pub(crate) use_dither_map: bool,
    pub(crate) fast_palette: bool,
    pub(crate) profile: Profile,
    speed: u8,
    log_callback: Option<LogCallback>,
    log_flush_callback: Option<LogFlushCallback>,
}

pub(crate) fn quality_to_mse(quality: u32) -> f64 {
    if quality == 0 {
        return MAX_DIFF;
    }
    if quality == 100 {
        return 0.0;
    }
    let quality = f64::from(quality);
    // curve fudged to be roughly similar to quality of libjpeg
    // except lowest 10 for really low number of colors
    let extra_low_quality_fudge = cmax(0.0, 0.016 / (0.001 + quality) - 0.001);
    extra_low_quality_fudge + 2.5 / (210.0 + quality).powf(1.2) * (100.1 - quality) / 100.0
}

pub(crate) fn mse_to_quality(mse: f64) -> u32 {
    // + epsilon for floating point errors
    (1..=100)
        .rev()
        .find(|&q| mse <= quality_to_mse(q) + 0.000_001)
        .unwrap_or(0)
}

impl Default for Attributes {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Attributes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attributes")
            .field("max_colors", &self.max_colors)
            .field("speed", &self.speed)
            .field("quality", &self.quality())
            .field("min_opacity", &self.min_opacity())
            .field("min_posterization", &self.min_posterization_output)
            .field("last_index_transparent", &self.last_index_transparent)
            .finish_non_exhaustive()
    }
}

impl Attributes {
    /// Default settings: 256 colors, speed 3, no quality limit, [`Profile::Improved`].
    #[must_use]
    pub fn new() -> Self {
        let mut attr = Self {
            target_mse: 0.0,
            max_mse: MAX_DIFF,
            voronoi_iteration_limit: 0.0,
            // whether to preserve opaque colors for IE (1.0=no, does not affect alpha)
            min_opaque_val: 1.0,
            max_colors: 256,
            max_histogram_entries: 0,
            min_posterization_output: 0,
            min_posterization_input: 0,
            voronoi_iterations: 0,
            hartigan_passes: 0,
            reorder_palette: false,
            feedback_loop_trials: 0,
            last_index_transparent: false,
            use_contrast_maps: false,
            use_dither_map: false,
            fast_palette: false,
            profile: Profile::default(),
            speed: 0,
            log_callback: None,
            log_flush_callback: None,
        };
        attr.set_speed(3).expect("valid speed");
        attr
    }

    /// Maximum number of colors in the palette, 2-256. Default 256.
    ///
    /// The palette can have fewer colors when the image has fewer, or when fewer reach the
    /// target quality set with [`set_quality`](Self::set_quality).
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] outside 2-256.
    pub fn set_max_colors(&mut self, colors: u32) -> Result<(), Error> {
        if !(2..=256).contains(&colors) {
            return Err(Error::ValueOutOfRange);
        }
        self.max_colors = colors;
        Ok(())
    }

    /// Maximum number of colors in the palette.
    #[must_use]
    pub fn max_colors(&self) -> u32 {
        self.max_colors
    }

    /// Quality range 0-100, like JPEG quality. Quantization uses the fewest colors that reach
    /// `target`; if even `max_colors` can't reach `minimum`, quantization fails with
    /// [`Error::QualityTooLow`]. The default, `(0, 100)`, uses up to `max_colors` colors and
    /// never fails for quality.
    ///
    /// ```
    /// use libimgquant::{Error, RGBA};
    ///
    /// // A smooth gradient can't look good with only 2 colors.
    /// let (width, height) = (64, 64);
    /// let pixels: Vec<RGBA> = (0..width * height)
    ///     .map(|i| RGBA::new((i % width * 4) as u8, (i / width * 4) as u8, 90, 255))
    ///     .collect();
    ///
    /// let mut attr = libimgquant::new();
    /// attr.set_max_colors(2)?;
    /// attr.set_quality(80, 100)?;
    /// let mut image = attr.new_image(pixels, width, height, 0.0)?;
    /// assert_eq!(attr.quantize(&mut image).err(), Some(Error::QualityTooLow));
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] if `target` is over 100 or below `minimum`.
    pub fn set_quality(&mut self, minimum: u8, target: u8) -> Result<(), Error> {
        if target > 100 || target < minimum {
            return Err(Error::ValueOutOfRange);
        }
        self.target_mse = quality_to_mse(target.into());
        self.max_mse = quality_to_mse(minimum.into());
        Ok(())
    }

    /// Quality range as `(minimum, target)`, as set with [`set_quality`](Self::set_quality).
    #[must_use]
    pub fn quality(&self) -> (u8, u8) {
        (
            mse_to_quality(self.max_mse) as u8,
            mse_to_quality(self.target_mse) as u8,
        )
    }

    /// Speed 1 (slowest, best quality) to 10 (fastest). Default 3.
    ///
    /// Speed sets how much refinement is done and which optional steps run. Speeds 1-5 use a
    /// dither map that keeps dithering away from edges. With [`Profile::Improved`], speeds 1-7
    /// also order the palette for compression.
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] outside 1-10.
    pub fn set_speed(&mut self, speed: i32) -> Result<(), Error> {
        if !(1..=10).contains(&speed) {
            return Err(Error::ValueOutOfRange);
        }
        let mut iterations = cmax(8 - speed, 0);
        iterations += iterations * iterations / 2;
        let mut trials = cmax(56 - 9 * speed, 0);
        self.hartigan_passes = 0;
        if self.profile == Profile::Improved {
            // The better median cut and Hartigan refinement make most of the repeated median
            // cuts and Lloyd iterations unnecessary; these were chosen by measuring time,
            // SSIMULACRA2, CIEDE2000 and PNG size on a corpus of photos, sprites and UI.
            (trials, iterations, self.hartigan_passes) = match speed {
                ..=2 => (1, 2, 3),
                3..=6 => (0, 1, 2),
                _ => (0, 1, 0),
            };
        }
        self.reorder_palette = self.profile == Profile::Improved && speed <= 7;
        self.voronoi_iterations = iterations as u32;
        self.voronoi_iteration_limit = 1.0 / f64::from(1u32 << (23 - speed));
        self.feedback_loop_trials = trials as u32;

        self.max_histogram_entries = (1 << 17) + (1 << 18) * (10 - speed) as u32;
        self.min_posterization_input = u8::from(speed >= 8);
        self.fast_palette = speed >= 7;
        // The original raised this to 7 in multi-threaded builds, making output depend on the
        // build. The single-threaded threshold is used regardless of threading.
        self.use_dither_map = speed <= 5;
        self.use_contrast_maps = speed <= 7 || self.use_dither_map;
        self.speed = speed as u8;
        Ok(())
    }

    /// Selects the algorithms to use (default [`Profile::Improved`]). Settings are kept; speed
    /// presets are re-derived for the profile.
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = profile;
        self.set_speed(self.speed.into()).expect("current speed is valid");
    }

    /// The selected algorithms.
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Speed setting, 1-10.
    #[must_use]
    pub fn speed(&self) -> u32 {
        self.speed.into()
    }

    /// Alpha values at or above `min` are made fully opaque, which works around IE6 transparency bugs.
    /// Default 255 (disabled).
    ///
    /// # Errors
    ///
    /// Never fails; the `Result` mirrors the C API.
    pub fn set_min_opacity(&mut self, min: u8) -> Result<(), Error> {
        self.min_opaque_val = (f64::from(min) / 255.0) as f32;
        Ok(())
    }

    /// Minimum opacity setting.
    #[must_use]
    pub fn min_opacity(&self) -> u8 {
        cmin(255.0, 256.0 * f64::from(self.min_opaque_val)) as u8
    }

    /// Ignores the given number of least significant bits in all channels (0-4),
    /// for output on displays with lower color depth. Default 0.
    ///
    /// # Errors
    ///
    /// [`Error::ValueOutOfRange`] above 4.
    pub fn set_min_posterization(&mut self, bits: u8) -> Result<(), Error> {
        if bits > 4 {
            return Err(Error::ValueOutOfRange);
        }
        self.min_posterization_output = bits;
        Ok(())
    }

    /// Minimum posterization setting.
    #[must_use]
    pub fn min_posterization(&self) -> u8 {
        self.min_posterization_output
    }

    /// Puts the fully transparent color last in the palette instead of first.
    /// This is a workaround for Blu-ray subtitles.
    pub fn set_last_index_transparent(&mut self, is_last: bool) {
        self.last_index_transparent = is_last;
    }

    /// Receives progress messages, which are only useful for debugging.
    ///
    /// With the `threads` feature the callback may be called from worker threads.
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    ///
    /// let log = Arc::new(Mutex::new(Vec::<String>::new()));
    /// let mut attr = libimgquant::new();
    /// let sink = Arc::clone(&log);
    /// attr.set_log_callback(move |_attr, message| sink.lock().unwrap().push(message.to_owned()));
    ///
    /// let pixels = vec![libimgquant::RGBA::new(10, 20, 30, 255); 16];
    /// let mut image = attr.new_image(pixels, 4, 4, 0.0)?;
    /// attr.quantize(&mut image)?;
    /// assert!(!log.lock().unwrap().is_empty());
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    pub fn set_log_callback<F: Fn(&Self, &str) + Send + Sync + 'static>(&mut self, callback: F) {
        self.flush_log();
        self.log_callback = Some(Arc::new(callback));
    }

    /// Removes the log callback.
    pub fn clear_log_callback(&mut self) {
        self.flush_log();
        self.log_callback = None;
    }

    /// Called when buffered log output should be flushed, including when the attributes are dropped.
    pub fn set_log_flush_callback<F: Fn(&Self) + Send + Sync + 'static>(&mut self, callback: F) {
        self.log_flush_callback = Some(Arc::new(callback));
    }

    /// Removes the log flush callback.
    pub fn clear_log_flush_callback(&mut self) {
        self.log_flush_callback = None;
    }

    pub(crate) fn verbose_print(&self, message: &str) {
        if let Some(cb) = &self.log_callback {
            cb(self, message);
        }
    }

    pub(crate) fn log_error(&self, message: &str) {
        if self.log_callback.is_some() {
            self.verbose_print(&format!("  error: {message}"));
        }
    }

    fn flush_log(&self) {
        if let Some(cb) = &self.log_flush_callback {
            cb(self);
        }
    }

    /// Creates an image that owns its pixels: `width * height` pixels, row by row.
    ///
    /// `gamma` is the gamma of the pixels; 0 means the sRGB default (0.45455).
    ///
    /// # Errors
    ///
    /// - [`Error::ValueOutOfRange`] if `width` or `height` is 0, the image has more than
    ///   `i32::MAX` pixels, or `gamma` is outside 0-1.
    /// - [`Error::BufferTooSmall`] if there are fewer than `width * height` pixels.
    pub fn new_image(
        &self,
        pixels: impl Into<Box<[RGBA]>>,
        width: usize,
        height: usize,
        gamma: f64,
    ) -> Result<Image<'static>, Error> {
        self.new_image_stride(pixels, width, height, width, gamma)
    }

    /// Like [`new_image`](Self::new_image), with `stride` pixels between the starts of rows.
    ///
    /// # Errors
    ///
    /// As [`new_image`](Self::new_image); also [`Error::BufferTooSmall`] if `stride` is less
    /// than `width` or the buffer ends before the last row.
    pub fn new_image_stride(
        &self,
        pixels: impl Into<Box<[RGBA]>>,
        width: usize,
        height: usize,
        stride: usize,
        gamma: f64,
    ) -> Result<Image<'static>, Error> {
        let source = Source::Contiguous {
            data: PixelData::Owned(pixels.into()),
            stride,
        };
        Image::new_internal(self, source, width, height, gamma)
    }

    /// Creates an image that borrows its pixels: `width * height` pixels, row by row.
    ///
    /// # Errors
    ///
    /// As [`new_image`](Self::new_image).
    pub fn new_image_borrowed<'a>(
        &self,
        pixels: &'a [RGBA],
        width: usize,
        height: usize,
        gamma: f64,
    ) -> Result<Image<'a>, Error> {
        self.new_image_stride_borrowed(pixels, width, height, width, gamma)
    }

    /// Like [`new_image_borrowed`](Self::new_image_borrowed), with `stride` pixels between the starts of rows.
    ///
    /// ```
    /// use libimgquant::RGBA;
    ///
    /// // A 3x2 image stored with 4 pixels per row; the padding pixel is ignored.
    /// let black = RGBA::new(0, 0, 0, 255);
    /// let white = RGBA::new(255, 255, 255, 255);
    /// let padding = RGBA::new(255, 0, 0, 255);
    /// let buffer = [black, white, black, padding, white, black, white, padding];
    ///
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image_stride_borrowed(&buffer, 3, 2, 4, 0.0)?;
    /// let (palette, _) = attr.quantize(&mut image)?.remapped(&mut image)?;
    /// assert_eq!(palette.len(), 2);
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`new_image_stride`](Self::new_image_stride).
    pub fn new_image_stride_borrowed<'a>(
        &self,
        pixels: &'a [RGBA],
        width: usize,
        height: usize,
        stride: usize,
        gamma: f64,
    ) -> Result<Image<'a>, Error> {
        let source = Source::Contiguous {
            data: PixelData::Borrowed(pixels),
            stride,
        };
        Image::new_internal(self, source, width, height, gamma)
    }

    /// Creates an image from separate rows, each at least `width` pixels long. Only the first
    /// `height` rows and the first `width` pixels of each are used.
    ///
    /// ```
    /// use libimgquant::RGBA;
    ///
    /// let top = vec![RGBA::new(200, 0, 0, 255); 8];
    /// let bottom = vec![RGBA::new(0, 0, 200, 255); 8];
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image_rows(vec![&top[..], &bottom[..]], 8, 2, 0.0)?;
    /// let mut result = attr.quantize(&mut image)?;
    /// let (_, indices) = result.remapped(&mut image)?;
    /// assert_ne!(indices[0], indices[8]);
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`new_image`](Self::new_image); [`Error::BufferTooSmall`] if there are fewer than
    /// `height` rows or a row is shorter than `width`.
    pub fn new_image_rows<'a>(
        &self,
        rows: impl Into<Box<[&'a [RGBA]]>>,
        width: usize,
        height: usize,
        gamma: f64,
    ) -> Result<Image<'a>, Error> {
        Image::new_internal(self, Source::Rows(rows.into()), width, height, gamma)
    }

    /// Creates an image whose rows are produced on demand by `callback(row_out, row_index)`,
    /// which must fill all `width` pixels of `row_out`.
    ///
    /// The callback may be called several times for the same row, and with the `threads`
    /// feature it may be called from several threads at once.
    ///
    /// ```
    /// use libimgquant::RGBA;
    ///
    /// let (width, height) = (32, 16);
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image_fn(
    ///     |row: &mut [RGBA], y: usize| {
    ///         for (x, px) in row.iter_mut().enumerate() {
    ///             *px = RGBA::new((x * 8) as u8, (y * 16) as u8, 0, 255);
    ///         }
    ///     },
    ///     width,
    ///     height,
    ///     0.0,
    /// )?;
    /// let (_, indices) = attr.quantize(&mut image)?.remapped(&mut image)?;
    /// assert_eq!(indices.len(), width * height);
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`new_image`](Self::new_image).
    pub fn new_image_fn<'a, F>(&self, callback: F, width: usize, height: usize, gamma: f64) -> Result<Image<'a>, Error>
    where
        F: Fn(&mut [RGBA], usize) + Send + Sync + 'a,
    {
        Image::new_internal(self, Source::Callback(Box::new(callback)), width, height, gamma)
    }

    /// Generates a palette for the image. The image is only borrowed; remap it (or other
    /// images) with the returned [`QuantizationResult`].
    ///
    /// # Errors
    ///
    /// - [`Error::QualityTooLow`] if the palette can't reach the minimum quality set with
    ///   [`set_quality`](Self::set_quality).
    /// - [`Error::OutOfMemory`] if a large buffer can't be allocated.
    /// - [`Error::ValueOutOfRange`] in edge cases where no palette can be made.
    pub fn quantize(&self, image: &mut Image<'_>) -> Result<QuantizationResult, Error> {
        image.ensure_color_space(self.profile.color_space());
        let mut hist = get_histogram(image, self)?;
        pngquant_quantize(&mut hist, self, image)
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        self.flush_log();
    }
}
