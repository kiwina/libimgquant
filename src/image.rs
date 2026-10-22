//! Input image and its row accessors.

use crate::RGBA;
use crate::attr::Attributes;
use crate::blur::{blur, for_each_row, max3, min3};
use crate::error::Error;
use crate::pal::{ColorLut, ColorSpace, FPixel, HIGH_MEMORY_LIMIT, cmax, cmin};

/// Callback that fills one row of RGBA pixels: `(row_out, row_index)`. See
/// [`Attributes::new_image_fn`].
pub type RowCallback<'a> = dyn Fn(&mut [RGBA], usize) + Send + Sync + 'a;

pub(crate) enum PixelData<'a> {
    Owned(Box<[RGBA]>),
    Borrowed(&'a [RGBA]),
}

pub(crate) enum Source<'a> {
    Contiguous { data: PixelData<'a>, stride: usize },
    Rows(Box<[&'a [RGBA]]>),
    Callback(Box<RowCallback<'a>>),
}

/// Scratch buffers for converting rows on the fly.
#[derive(Default)]
pub(crate) struct RowTemp {
    rgba: Vec<RGBA>,
    f: Vec<FPixel>,
}

/// An image to quantize or remap. Create it with [`Attributes::new_image`] and friends.
///
/// The image caches data derived from its pixels (converted colors, contrast maps),
/// so it is passed mutably to [`Attributes::quantize`] and the remapping functions.
///
/// The lifetime `'a` is that of borrowed pixels, rows or callbacks; images that own their
/// pixels are `Image<'static>`.
pub struct Image<'a> {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) gamma: f64,
    source: Source<'a>,
    /// Conversion to internal coordinates; caches below are in its color space.
    lut: ColorLut,
    f_pixels: Option<Box<[FPixel]>>,
    /// Converted pixels are not cached; rows are converted when needed.
    low_memory: bool,
    pub(crate) noise: Option<Box<[u8]>>,
    pub(crate) edges: Option<Box<[u8]>>,
    pub(crate) dither_map: Option<Box<[u8]>>,
    pub(crate) min_opaque_val: f32,
    pub(crate) fixed_colors: Vec<FPixel>,
    fixed_rgba: Vec<RGBA>,
}

fn should_use_low_memory(pixels: usize, low_memory_hint: bool) -> bool {
    let limit = if low_memory_hint {
        HIGH_MEMORY_LIMIT / 8
    } else {
        HIGH_MEMORY_LIMIT
    };
    pixels > limit / std::mem::size_of::<FPixel>()
}

/// Allocates `len` copies of `value`, or fails instead of aborting when memory is short.
/// With the `threads` feature the buffer is filled in parallel, so the page faults of a
/// large fresh allocation are spread over the worker threads.
fn try_filled<T: Clone + Send + Sync>(len: usize, value: T) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| Error::OutOfMemory)?;
    #[cfg(feature = "threads")]
    {
        use rayon::prelude::*;
        v.par_extend((0..len).into_par_iter().with_min_len(1 << 16).map(|_| value.clone()));
    }
    #[cfg(not(feature = "threads"))]
    v.resize(len, value);
    Ok(v)
}

impl<'a> Image<'a> {
    pub(crate) fn new_internal(
        attr: &Attributes,
        source: Source<'a>,
        width: usize,
        height: usize,
        gamma: f64,
    ) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            attr.log_error("width and height must be > 0");
            return Err(Error::ValueOutOfRange);
        }
        // same limit as the original; it keeps all per-pixel counters and sizes in range
        if width > i32::MAX as usize / height {
            attr.log_error("image too large");
            return Err(Error::ValueOutOfRange);
        }
        let pixels = width * height;
        if !(0.0..=1.0).contains(&gamma) {
            attr.log_error("gamma must be >= 0 and <= 1 (try 1/gamma instead)");
            return Err(Error::ValueOutOfRange);
        }
        let sufficient = match &source {
            Source::Contiguous { data, stride } => {
                let len = match data {
                    PixelData::Owned(d) => d.len(),
                    PixelData::Borrowed(d) => d.len(),
                };
                *stride >= width
                    && stride
                        .checked_mul(height - 1)
                        .and_then(|s| s.checked_add(width))
                        .is_some_and(|n| n <= len)
            }
            Source::Rows(rows) => rows.len() >= height && rows[..height].iter().all(|r| r.len() >= width),
            Source::Callback(_) => true,
        };
        if !sufficient {
            attr.log_error("pixel buffer is smaller than width and height");
            return Err(Error::BufferTooSmall);
        }

        let gamma = if gamma == 0.0 { 0.45455 } else { gamma };
        let has_rows = !matches!(source, Source::Callback(_));
        // if image is huge or converted pixels are not likely to be reused then don't cache converted pixels
        let low_memory_hint = has_rows && attr.min_opaque_val >= 1.0 && !attr.use_contrast_maps && !attr.use_dither_map;
        let low_memory = should_use_low_memory(pixels, low_memory_hint);
        if low_memory {
            attr.verbose_print("  conserving memory");
        }
        if attr.min_opaque_val < 1.0 {
            attr.verbose_print("  Working around IE6 bug by making image less transparent...");
        }

        Ok(Self {
            width,
            height,
            gamma,
            source,
            lut: ColorLut::new(gamma, attr.profile.color_space()),
            f_pixels: None,
            low_memory,
            noise: None,
            edges: None,
            dither_map: None,
            min_opaque_val: attr.min_opaque_val,
            fixed_colors: Vec::new(),
            fixed_rgba: Vec::new(),
        })
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Reserves a color that will always be included in the palette (up to 256 colors).
    ///
    /// Fixed colors count towards [`Attributes::set_max_colors`]. Add them before quantizing.
    ///
    /// ```
    /// use libimgquant::RGBA;
    ///
    /// let pixels: Vec<RGBA> = (0..64u8).map(|i| RGBA::new(i * 4, 100, 50, 255)).collect();
    /// let attr = libimgquant::new();
    /// let mut image = attr.new_image(pixels, 8, 8, 0.0)?;
    /// let brand = RGBA::new(0, 128, 255, 255);
    /// image.add_fixed_color(brand)?;
    /// let (palette, _) = attr.quantize(&mut image)?.remapped(&mut image)?;
    /// assert!(palette.contains(&brand));
    /// # Ok::<(), libimgquant::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooSmall`] if the image already has 256 fixed colors.
    pub fn add_fixed_color(&mut self, color: RGBA) -> Result<(), Error> {
        if self.fixed_colors.len() > 255 {
            return Err(Error::BufferTooSmall);
        }
        self.fixed_colors.push(FPixel::from_rgba(&self.lut, color));
        self.fixed_rgba.push(color);
        Ok(())
    }

    pub(crate) fn lut(&self) -> &ColorLut {
        &self.lut
    }

    /// Switches internal coordinates to `space` (an image can be quantized or remapped with
    /// another profile than the one it was created with), dropping caches made in the old one.
    pub(crate) fn ensure_color_space(&mut self, space: ColorSpace) {
        if self.lut.space() == space {
            return;
        }
        self.lut = ColorLut::new(self.gamma, space);
        self.f_pixels = None;
        self.noise = None;
        self.edges = None;
        self.dither_map = None;
        self.fixed_colors = self
            .fixed_rgba
            .iter()
            .map(|&c| FPixel::from_rgba(&self.lut, c))
            .collect();
    }

    fn direct_row(&self, row: usize) -> Option<&[RGBA]> {
        let w = self.width;
        match &self.source {
            Source::Contiguous { data, stride } => {
                let data = match data {
                    PixelData::Owned(d) => &d[..],
                    PixelData::Borrowed(d) => d,
                };
                Some(&data[row * stride..row * stride + w])
            }
            Source::Rows(rows) => Some(&rows[row][..w]),
            Source::Callback(_) => None,
        }
    }

    /// Rows can be borrowed from the source as they are (no callback, no opacity adjustment).
    fn has_direct_rows(&self) -> bool {
        self.min_opaque_val >= 1.0 && !matches!(self.source, Source::Callback(_))
    }

    /// Returns RGBA pixels of a row, with the minimum opacity adjustment applied.
    pub(crate) fn row_rgba<'s>(&'s self, row: usize, temp: &'s mut Vec<RGBA>) -> &'s [RGBA] {
        if self.has_direct_rows() {
            if let Some(r) = self.direct_row(row) {
                return r;
            }
        }
        self.fill_row_rgba(row, temp);
        temp
    }

    /// Copies a row into `temp`, with the minimum opacity adjustment applied.
    fn fill_row_rgba(&self, row: usize, temp: &mut Vec<RGBA>) {
        let iebug = self.min_opaque_val < 1.0;
        temp.clear();
        match self.direct_row(row) {
            Some(r) => temp.extend_from_slice(r),
            None => {
                temp.resize(self.width, RGBA::new(0, 0, 0, 0));
                if let Source::Callback(cb) = &self.source {
                    cb(temp, row);
                }
            }
        }
        if iebug {
            self.modify_alpha(temp);
        }
    }

    /// Returns a row of pixels in internal color space.
    pub(crate) fn row_f<'s>(&'s self, row: usize, temp: &'s mut RowTemp) -> &'s [FPixel] {
        if let Some(f) = &self.f_pixels {
            return &f[row * self.width..(row + 1) * self.width];
        }
        let RowTemp { rgba, f } = temp;
        let pixels = self.row_rgba(row, rgba);
        f.clear();
        f.extend(pixels.iter().map(|&px| FPixel::from_rgba(&self.lut, px)));
        f
    }

    /// Converts all pixels once and caches them, unless the image is too large.
    pub(crate) fn prepare_f_pixels(&mut self) {
        if self.f_pixels.is_some() || self.low_memory {
            return;
        }
        let n = self.width * self.height;
        if should_use_low_memory(n, false) {
            self.low_memory = true;
            return;
        }
        let mut buf = Vec::new();
        if buf.try_reserve_exact(n).is_err() {
            self.low_memory = true;
            return;
        }
        // Converted straight into the reserved buffer, without zero-filling it first: a serial
        // fill of a fresh 16-bytes-per-pixel buffer is mostly page faults on one thread.
        self.convert_f_pixels(&mut buf);
        debug_assert_eq!(buf.len(), n);
        self.f_pixels = Some(buf.into_boxed_slice());
    }

    /// Appends all pixels, converted to internal color space, to `buf` (with capacity reserved).
    #[cfg(not(feature = "threads"))]
    fn convert_f_pixels(&self, buf: &mut Vec<FPixel>) {
        let mut temp = Vec::new();
        for row in 0..self.height {
            let pixels = self.row_rgba(row, &mut temp);
            buf.extend(pixels.iter().map(|&px| FPixel::from_rgba(&self.lut, px)));
        }
    }

    /// Converts all pixels in parallel into `buf` (with capacity reserved). The pixels are
    /// produced by an indexed parallel iterator, so each worker writes (and first touches)
    /// its own part of the buffer. Each task keeps the row it is converting, so the row
    /// index is only computed when the task crosses into another row.
    #[cfg(feature = "threads")]
    fn convert_f_pixels(&self, buf: &mut Vec<FPixel>) {
        use rayon::prelude::*;
        let w = self.width;
        let n = w * self.height;
        let lut = &self.lut;
        let pixels = (0..n).into_par_iter().with_min_len(4096);
        if self.has_direct_rows() {
            pixels
                .map_init(
                    || (0..0, &[][..]),
                    |(range, row), i| {
                        if !range.contains(&i) {
                            let r = i / w;
                            *range = r * w..(r + 1) * w;
                            *row = self.direct_row(r).expect("rows are borrowed");
                        }
                        FPixel::from_rgba(lut, row[i - range.start])
                    },
                )
                .collect_into_vec(buf);
        } else {
            pixels
                .map_init(
                    || (0..0, Vec::new()),
                    |(range, row), i| {
                        if !range.contains(&i) {
                            let r = i / w;
                            *range = r * w..(r + 1) * w;
                            self.fill_row_rgba(r, row);
                        }
                        FPixel::from_rgba(lut, row[i - range.start])
                    },
                )
                .collect_into_vec(buf);
        }
    }

    /// IE6 makes colors with even slightest transparency completely transparent,
    /// thus to improve situation in IE, make colors that are less than ~10% transparent completely opaque.
    fn modify_alpha(&self, row_pixels: &mut [RGBA]) {
        let min_opaque_val = self.min_opaque_val;
        let almost_opaque_val = min_opaque_val * 169.0 / 256.0;
        let almost_opaque_val_int = (min_opaque_val * 169.0 / 256.0 * 255.0) as u32;

        for px in row_pixels {
            // to avoid visible step caused by forced opaqueness, linearly raise opaqueness of almost-opaque colors
            if u32::from(px.a) >= almost_opaque_val_int {
                let mut al = f32::from(px.a) / 255.0;
                al = almost_opaque_val
                    + (al - almost_opaque_val) * (1.0 - almost_opaque_val) / (min_opaque_val - almost_opaque_val);
                al *= 256.0;
                px.a = if al >= 255.0 { 255 } else { al as u8 };
            }
        }
    }

    /// Computes row `j` of the noise and edge maps (before filtering) from rows j-1, j, j+1.
    fn contrast_row(&self, j: usize, temp: &mut [RowTemp; 3], noise: &mut [u8], edges: &mut [u8]) {
        let (cols, rows) = (self.width, self.height);
        let [t0, t1, t2] = temp;
        let prev_row = self.row_f(j.saturating_sub(1), t0);
        let curr_row = self.row_f(j, t1);
        let next_row = self.row_f(cmin(rows - 1, j + 1), t2);

        let mut curr = curr_row[0];
        let mut next = curr;
        for i in 0..cols {
            let prev = curr;
            curr = next;
            next = curr_row[cmin(cols - 1, i + 1)];

            // contrast is difference between pixels neighbouring horizontally and vertically
            let a = (prev.a + next.a - curr.a * 2.0).abs();
            let r = (prev.r + next.r - curr.r * 2.0).abs();
            let g = (prev.g + next.g - curr.g * 2.0).abs();
            let b = (prev.b + next.b - curr.b * 2.0).abs();

            let prevl = prev_row[i];
            let nextl = next_row[i];

            let a1 = (prevl.a + nextl.a - curr.a * 2.0).abs();
            let r1 = (prevl.r + nextl.r - curr.r * 2.0).abs();
            let g1 = (prevl.g + nextl.g - curr.g * 2.0).abs();
            let b1 = (prevl.b + nextl.b - curr.b * 2.0).abs();

            let horiz = cmax(cmax(a, r), cmax(g, b));
            let vert = cmax(cmax(a1, r1), cmax(g1, b1));
            let edge = cmax(horiz, vert);
            let mut z = edge - (horiz - vert).abs() * 0.5;
            z = 1.0 - cmax(z, cmin(horiz, vert));
            z *= z; // noise is amplified
            z *= z;

            z *= 256.0;
            noise[i] = if z < 256.0 { z as u8 } else { 255 };
            // edge can exceed 1 for high-contrast pixels. The original converted the negative value
            // with undefined behavior (wrapping on x86); strong edges must not be dithered.
            z = (1.0 - edge) * 256.0;
            edges[i] = if z < 0.0 {
                0
            } else if z < 256.0 {
                z as u8
            } else {
                255
            };
        }
    }

    /// Builds two maps:
    /// - noise: approximation of areas with high-frequency noise, except straight edges. 255=flat, 0=noisy.
    /// - edges: noise map including all edges.
    pub(crate) fn contrast_maps(&mut self) {
        let (cols, rows) = (self.width, self.height);
        if cols < 4 || rows < 4 || (cols * rows).saturating_mul(3) > HIGH_MEMORY_LIMIT {
            return;
        }
        let (Ok(mut noise), Ok(mut edges), Ok(mut tmp)) = (
            try_filled(cols * rows, 0u8),
            try_filled(cols * rows, 0u8),
            try_filled(cols * rows, 0u8),
        ) else {
            return;
        };

        self.prepare_f_pixels();
        // Uncached images (over ~4 megapixels) differ from 2.4.1 here: it returned one shared
        // row buffer, so its previous, current and next rows were all row j+1 and vertical
        // contrast was always 0. The real neighbouring rows are used for every image size.
        // Each output row depends only on input rows j-1, j and j+1, so rows run in parallel.
        let this: &Self = self;
        #[cfg(feature = "threads")]
        {
            use rayon::prelude::*;
            noise
                .par_chunks_mut(cols)
                .zip(edges.par_chunks_mut(cols))
                .with_min_len((4096 / cols).max(1))
                .enumerate()
                .for_each_init(<[RowTemp; 3]>::default, |temp, (j, (noise, edges))| {
                    this.contrast_row(j, temp, noise, edges);
                });
        }
        #[cfg(not(feature = "threads"))]
        {
            let mut temp = <[RowTemp; 3]>::default();
            for (j, (noise, edges)) in noise.chunks_mut(cols).zip(edges.chunks_mut(cols)).enumerate() {
                this.contrast_row(j, &mut temp, noise, edges);
            }
        }

        // noise areas are shrunk and then expanded to remove thin edges from the map
        max3(&noise, &mut tmp, cols, rows);
        max3(&tmp, &mut noise, cols, rows);

        blur(&mut noise, &mut tmp, cols, rows, 3);

        max3(&noise, &mut tmp, cols, rows);

        min3(&tmp, &mut noise, cols, rows);
        min3(&noise, &mut tmp, cols, rows);
        min3(&tmp, &mut noise, cols, rows);

        min3(&edges, &mut tmp, cols, rows);
        max3(&tmp, &mut edges, cols, rows);
        for_each_row(
            &mut edges,
            cols,
            || (),
            |(), j, edges| {
                for (e, &n) in edges.iter_mut().zip(&noise[j * cols..(j + 1) * cols]) {
                    *e = cmin(n, *e);
                }
            },
        );

        self.noise = Some(noise.into_boxed_slice());
        self.edges = Some(edges.into_boxed_slice());
    }
}
