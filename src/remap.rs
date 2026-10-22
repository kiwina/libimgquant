//! Remapping of images to a palette, with optional Floyd-Steinberg dithering.

use crate::attr::Profile;
use crate::image::{Image, RowTemp};
use crate::kmeans::Kmeans;
use crate::nearest::Nearest;
use crate::pal::{FPixel, PalF, cmin};
use crate::rng::{DitherRng, RAND_MAX};

/// Maps each pixel to the nearest palette color, and moves the palette towards the
/// colors that map to it. Returns the mean remapping error.
pub(crate) fn remap_to_palette(
    image: &mut Image<'_>,
    output: &mut [&mut [u8]],
    map: &mut PalF,
    fast: bool,
    profile: Profile,
) -> f32 {
    image.prepare_f_pixels();
    let image: &Image<'_> = image;
    let (width, height) = (image.width, image.height);
    let min_opaque_val = image.min_opaque_val;
    let n = Nearest::new(map, fast, profile);
    let colors = map.len();

    let process = |first_row: usize, rows: &mut [&mut [u8]]| {
        let mut kmeans = Kmeans::new(colors);
        let mut remapping_error = 0f64;
        let mut temp = RowTemp::default();
        for (y, out_row) in rows.iter_mut().enumerate() {
            let row_pixels = image.row_f(first_row + y, &mut temp);
            let mut last_match = 0;
            for (out, &px) in out_row.iter_mut().zip(row_pixels) {
                let (matched, diff) = n.search(px, last_match, min_opaque_val);
                last_match = matched;
                *out = matched as u8;
                remapping_error += f64::from(diff);
                kmeans.update_color(px, 1.0, matched);
            }
        }
        (kmeans, remapping_error)
    };

    #[cfg(feature = "threads")]
    let (kmeans, remapping_error) = {
        use rayon::prelude::*;
        // fixed row grouping, so the result doesn't depend on the number of threads
        let rows_per_chunk = (65536 / width).max(1);
        if height > rows_per_chunk {
            let parts: Vec<_> = output
                .par_chunks_mut(rows_per_chunk)
                .enumerate()
                .map(|(i, rows)| process(i * rows_per_chunk, rows))
                .collect();
            let mut parts = parts.into_iter();
            let (mut kmeans, mut err) = parts.next().expect("non-empty");
            for (k, e) in parts {
                kmeans.merge(&k);
                err += e;
            }
            (kmeans, err)
        } else {
            process(0, output)
        }
    };
    #[cfg(not(feature = "threads"))]
    let (kmeans, remapping_error) = process(0, output);

    drop(n);
    kmeans.finalize(map);
    (remapping_error / (width * height) as f64) as f32
}

#[inline(always)]
fn get_dithered_pixel(dither_level: f32, max_dither_error: f32, thiserr: FPixel, px: FPixel) -> FPixel {
    // Use Floyd-Steinberg errors to adjust actual color.
    let sr = thiserr.r * dither_level;
    let sg = thiserr.g * dither_level;
    let sb = thiserr.b * dither_level;
    let sa = thiserr.a * dither_level;

    let mut ratio = 1.0f32;
    let mut limit = |v: f32, s: f32| {
        // allowing some overflow prevents undithered bands caused by clamping of all channels
        if f64::from(v + s) > 1.03 {
            ratio = cmin(f64::from(ratio), (1.03 - f64::from(v)) / f64::from(s)) as f32;
        } else if v + s < 0.0 {
            ratio = cmin(ratio, v / -s);
        }
    };
    limit(px.r, sr);
    limit(px.g, sg);
    limit(px.b, sb);

    let a = (px.a + sa).clamp(0.0, 1.0);

    // If dithering error is crazy high, don't propagate it that much.
    // This prevents crazy green pixels popping out of the blue (or red or black! ;)
    let dither_error = sr * sr + sg * sg + sb * sb + sa * sa;
    if dither_error > max_dither_error {
        ratio = (f64::from(ratio) * 0.8) as f32;
    } else if dither_error < 2.0 / 256.0 / 256.0 {
        // don't dither areas that don't have noticeable error — makes file smaller
        return px;
    }

    FPixel {
        r: px.r + sr * ratio,
        g: px.g + sg * ratio,
        b: px.b + sb * ratio,
        a,
    }
}

#[inline(always)]
fn add_err(target: &mut FPixel, err: FPixel, weight: f32) {
    target.a += err.a * weight;
    target.r += err.r * weight;
    target.g += err.g * weight;
    target.b += err.b * weight;
}

#[inline(always)]
fn set_err(target: &mut FPixel, err: FPixel, weight: f32) {
    *target = FPixel {
        a: err.a * weight,
        r: err.r * weight,
        g: err.g * weight,
        b: err.b * weight,
    };
}

/// Rows per strip of the strip-parallel dithering used by [`Profile::Improved`].
const STRIP_ROWS: usize = 64;
/// Rows dithered just above each strip (their output is discarded) so that the strip starts
/// with error buffers in a steady state instead of empty ones. Fewer than 8 rows leave a
/// measurable seam; at 8 the seam rows match the sequential pass within noise.
const WARMUP_ROWS: usize = 8;
const _: () = assert!(WARMUP_ROWS <= STRIP_ROWS);

/// Seed of the initial dithering noise (of strip 0 in [`Profile::Improved`]).
const NOISE_SEED: u32 = 12345;

/// Everything one Floyd-Steinberg pass needs that does not change from row to row.
struct Floyd<'a> {
    image: &'a Image<'a>,
    map: &'a PalF,
    n: Nearest,
    dither_map: Option<&'a [u8]>,
    cols: usize,
    min_opaque_val: f32,
    max_dither_error: f32,
    base_dithering_level: f32,
    /// The output already holds an undithered remapping, used as search hints.
    output_image_is_remapped: bool,
}

impl Floyd<'_> {
    /// Error buffers for the current and the next row. The current one starts with tiny
    /// seeded noise. (+2 saves from checking out of bounds access.)
    fn error_rows(&self, seed: u32) -> (Vec<FPixel>, Vec<FPixel>) {
        let mut thiserr = vec![FPixel::default(); self.cols + 2];
        let nexterr = thiserr.clone();
        // deterministic dithering is better for comparing results
        let mut rng = DitherRng::new(seed);
        let rand_max = f64::from(RAND_MAX);
        let mut noise = || ((f64::from(rng.next()) - rand_max / 2.0) / rand_max / 255.0) as f32;
        for e in &mut thiserr {
            e.r = noise();
            e.g = noise();
            e.b = noise();
            e.a = noise();
        }
        (thiserr, nexterr)
    }

    /// Dithers one row, left to right if `fs_direction` is true. Diffuses error within
    /// `thiserr` and into `nexterr` (which this clears first); the caller swaps them.
    /// If `output_image_is_remapped`, `out_row` must hold this row's undithered indices.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn row(
        &self,
        row: usize,
        fs_direction: bool,
        temp: &mut RowTemp,
        out_row: &mut [u8],
        last_match: &mut usize,
        thiserr: &mut [FPixel],
        nexterr: &mut [FPixel],
    ) {
        let (cols, map, max_dither_error) = (self.cols, self.map, self.max_dither_error);
        nexterr.fill(FPixel::default());

        let row_pixels = self.image.row_f(row, temp);
        let mut col = if fs_direction { 0 } else { cols - 1 };

        loop {
            let mut dither_level = self.base_dithering_level;
            if let Some(dm) = self.dither_map {
                dither_level *= f32::from(dm[row * cols + col]);
            }

            let spx = get_dithered_pixel(dither_level, max_dither_error, thiserr[col + 1], row_pixels[col]);

            let guessed_match = if self.output_image_is_remapped {
                usize::from(out_row[col])
            } else {
                *last_match
            };
            *last_match = self.n.search(spx, guessed_match, self.min_opaque_val).0;
            out_row[col] = *last_match as u8;

            let xp = map.items[*last_match].color;
            let mut err = FPixel {
                r: spx.r - xp.r,
                g: spx.g - xp.g,
                b: spx.b - xp.b,
                a: spx.a - xp.a,
            };

            // If dithering error is crazy high, don't propagate it that much
            if err.r * err.r + err.g * err.g + err.b * err.b + err.a * err.a > max_dither_error {
                dither_level = (f64::from(dither_level) * 0.75) as f32;
            }

            let colorimp = (3.0 + xp.a) / 4.0 * dither_level;
            err.r *= colorimp;
            err.g *= colorimp;
            err.b *= colorimp;
            err.a *= dither_level;

            // Propagate Floyd-Steinberg error terms.
            if fs_direction {
                add_err(&mut thiserr[col + 2], err, 7.0 / 16.0);
                set_err(&mut nexterr[col + 2], err, 1.0 / 16.0);
                add_err(&mut nexterr[col + 1], err, 5.0 / 16.0);
                add_err(&mut nexterr[col], err, 3.0 / 16.0);
            } else {
                add_err(&mut thiserr[col], err, 7.0 / 16.0);
                set_err(&mut nexterr[col], err, 1.0 / 16.0);
                add_err(&mut nexterr[col + 1], err, 5.0 / 16.0);
                add_err(&mut nexterr[col + 2], err, 3.0 / 16.0);
            }

            // remapping is done in zig-zag
            if fs_direction {
                col += 1;
                if col >= cols {
                    break;
                }
            } else {
                if col == 0 {
                    break;
                }
                col -= 1;
            }
        }
    }

    /// The 2.4.1 pass: one serpentine scan over the whole image.
    fn sequential(&self, output: &mut [&mut [u8]]) {
        let (mut thiserr, mut nexterr) = self.error_rows(NOISE_SEED);
        let mut temp = RowTemp::default();
        let mut fs_direction = true;
        let mut last_match = 0;
        for (row, out_row) in output[..self.image.height].iter_mut().enumerate() {
            self.row(
                row,
                fs_direction,
                &mut temp,
                out_row,
                &mut last_match,
                &mut thiserr,
                &mut nexterr,
            );
            std::mem::swap(&mut thiserr, &mut nexterr);
            fs_direction = !fs_direction;
        }
    }

    /// Strip-parallel pass. Error diffusion is sequential only through its error buffers,
    /// and the error a pixel receives from rows far above it is negligible. So the image is
    /// cut into strips of [`STRIP_ROWS`] rows, and each strip is dithered on its own: with
    /// fresh error buffers and noise seeded by its index, starting [`WARMUP_ROWS`] rows above
    /// its first row, whose output is thrown away. Rows alternate direction by their
    /// absolute index, as in the sequential scan (Ulichney, *Digital Halftoning*, 1987).
    ///
    /// Strips, warm-up rows and seeds depend only on the image height, so the result is the
    /// same for any number of threads, and the same without the `threads` feature, which
    /// runs the same strips one after another. The first strip is exactly the sequential
    /// pass, so images of up to [`STRIP_ROWS`] rows are dithered exactly as in 2.4.1.
    fn strips(&self, output: &mut [&mut [u8]]) {
        let cols = self.cols;
        let output = &mut output[..self.image.height];

        // Warm-up rows belong to the previous strip, which may be dithered already (or be
        // written by another thread), so their search hints are copied up front.
        let warmup_hints: Vec<u8> = if self.output_image_is_remapped {
            (STRIP_ROWS..output.len())
                .step_by(STRIP_ROWS)
                .flat_map(|first_row| output[first_row - WARMUP_ROWS..first_row].iter())
                .flat_map(|r| r.iter().copied())
                .collect()
        } else {
            Vec::new()
        };

        let strip = |index: usize, out: &mut [&mut [u8]]| {
            let first_row = index * STRIP_ROWS;
            let start = first_row.saturating_sub(WARMUP_ROWS);
            let (mut thiserr, mut nexterr) = self.error_rows(NOISE_SEED.wrapping_add(index as u32));
            let mut scratch = vec![0u8; if start < first_row { cols } else { 0 }];
            let mut temp = RowTemp::default();
            let mut last_match = 0;
            for row in start..first_row + out.len() {
                let out_row: &mut [u8] = match row.checked_sub(first_row) {
                    Some(i) => &mut *out[i],
                    None => {
                        if self.output_image_is_remapped {
                            let hint_row = (index - 1) * WARMUP_ROWS + (row - start);
                            scratch.copy_from_slice(&warmup_hints[hint_row * cols..(hint_row + 1) * cols]);
                        }
                        &mut scratch
                    }
                };
                self.row(
                    row,
                    row % 2 == 0,
                    &mut temp,
                    out_row,
                    &mut last_match,
                    &mut thiserr,
                    &mut nexterr,
                );
                std::mem::swap(&mut thiserr, &mut nexterr);
            }
        };

        #[cfg(feature = "threads")]
        {
            use rayon::prelude::*;
            output
                .par_chunks_mut(STRIP_ROWS)
                .enumerate()
                .for_each(|(index, out)| strip(index, out));
        }
        #[cfg(not(feature = "threads"))]
        for (index, out) in output.chunks_mut(STRIP_ROWS).enumerate() {
            strip(index, out);
        }
    }
}

/// Floyd-Steinberg dithering in a serpentine order (Floyd & Steinberg, "An adaptive
/// algorithm for spatial greyscale", Proc. SID 17(2), 1976). An edge/noise map limits
/// dithering to flat areas: dithering on edges creates jagged lines, and noisy areas are
/// "naturally" dithered.
///
/// [`Profile::Compat`] scans the whole image in one pass, as 2.4.1 did. [`Profile::Improved`]
/// dithers strips of rows independently (in parallel with the `threads` feature), see
/// [`Floyd::strips`].
///
/// If `output_image_is_remapped` is true, the existing output indices are used as search hints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remap_to_palette_floyd(
    image: &mut Image<'_>,
    output: &mut [&mut [u8]],
    map: &PalF,
    max_dither_error: f32,
    use_dither_map: bool,
    output_image_is_remapped: bool,
    base_dithering_level: f32,
    profile: Profile,
) {
    image.prepare_f_pixels();
    let image: &Image<'_> = image;
    let dither_map = if use_dither_map {
        image.dither_map.as_deref().or(image.edges.as_deref())
    } else {
        None
    };

    // response to this value is non-linear and without it any value < 0.8 would give almost no dithering
    let inv = 1.0 - f64::from(base_dithering_level);
    let mut base_dithering_level = (1.0 - inv * inv * inv) as f32;
    if dither_map.is_some() {
        base_dithering_level = (f64::from(base_dithering_level) * (1.0 / 255.0)) as f32; // convert byte to float
    }
    base_dithering_level = (f64::from(base_dithering_level) * (15.0 / 16.0)) as f32; // prevent small errors from accumulating

    let floyd = Floyd {
        image,
        map,
        n: Nearest::new(map, false, profile),
        dither_map,
        cols: image.width,
        min_opaque_val: image.min_opaque_val,
        max_dither_error,
        base_dithering_level,
        output_image_is_remapped,
    };
    match profile {
        Profile::Compat => floyd.sequential(output),
        Profile::Improved => floyd.strips(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RGBA;
    use crate::attr::Attributes;
    use crate::pal::{ColorLut, ColorSpace, PalItem};

    /// The 2.4.1 pass as it was before the strip split (only the noise seed and the
    /// profile's nearest-color search are parameters), to check that the refactored
    /// [`Profile::Compat`] path is unchanged and that strips are independent passes.
    #[allow(clippy::too_many_arguments)]
    fn reference_floyd(
        image: &mut Image<'_>,
        output: &mut [&mut [u8]],
        map: &PalF,
        max_dither_error: f32,
        use_dither_map: bool,
        output_image_is_remapped: bool,
        base_dithering_level: f32,
        seed: u32,
        profile: Profile,
    ) {
        image.prepare_f_pixels();
        let image: &Image<'_> = image;
        let (rows, cols) = (image.height, image.width);
        let dither_map = if use_dither_map {
            image.dither_map.as_deref().or(image.edges.as_deref())
        } else {
            None
        };
        let min_opaque_val = image.min_opaque_val;
        let n = Nearest::new(map, false, profile);

        let mut thiserr = vec![FPixel::default(); cols + 2];
        let mut nexterr = vec![FPixel::default(); cols + 2];
        let mut rng = DitherRng::new(seed);
        let rand_max = f64::from(RAND_MAX);
        let mut noise = || ((f64::from(rng.next()) - rand_max / 2.0) / rand_max / 255.0) as f32;
        for e in &mut thiserr {
            e.r = noise();
            e.g = noise();
            e.b = noise();
            e.a = noise();
        }

        let inv = 1.0 - f64::from(base_dithering_level);
        let mut base_dithering_level = (1.0 - inv * inv * inv) as f32;
        if dither_map.is_some() {
            base_dithering_level = (f64::from(base_dithering_level) * (1.0 / 255.0)) as f32;
        }
        base_dithering_level = (f64::from(base_dithering_level) * (15.0 / 16.0)) as f32;

        let mut temp = RowTemp::default();
        let mut fs_direction = true;
        let mut last_match = 0;
        for row in 0..rows {
            nexterr.fill(FPixel::default());

            let row_pixels = image.row_f(row, &mut temp);
            let out_row = &mut *output[row];
            let mut col = if fs_direction { 0 } else { cols - 1 };

            loop {
                let mut dither_level = base_dithering_level;
                if let Some(dm) = dither_map {
                    dither_level *= f32::from(dm[row * cols + col]);
                }

                let spx = get_dithered_pixel(dither_level, max_dither_error, thiserr[col + 1], row_pixels[col]);

                let guessed_match = if output_image_is_remapped {
                    usize::from(out_row[col])
                } else {
                    last_match
                };
                last_match = n.search(spx, guessed_match, min_opaque_val).0;
                out_row[col] = last_match as u8;

                let xp = map.items[last_match].color;
                let mut err = FPixel {
                    r: spx.r - xp.r,
                    g: spx.g - xp.g,
                    b: spx.b - xp.b,
                    a: spx.a - xp.a,
                };

                if err.r * err.r + err.g * err.g + err.b * err.b + err.a * err.a > max_dither_error {
                    dither_level = (f64::from(dither_level) * 0.75) as f32;
                }

                let colorimp = (3.0 + xp.a) / 4.0 * dither_level;
                err.r *= colorimp;
                err.g *= colorimp;
                err.b *= colorimp;
                err.a *= dither_level;

                if fs_direction {
                    add_err(&mut thiserr[col + 2], err, 7.0 / 16.0);
                    set_err(&mut nexterr[col + 2], err, 1.0 / 16.0);
                    add_err(&mut nexterr[col + 1], err, 5.0 / 16.0);
                    add_err(&mut nexterr[col], err, 3.0 / 16.0);
                } else {
                    add_err(&mut thiserr[col], err, 7.0 / 16.0);
                    set_err(&mut nexterr[col], err, 1.0 / 16.0);
                    add_err(&mut nexterr[col + 1], err, 5.0 / 16.0);
                    add_err(&mut nexterr[col + 2], err, 3.0 / 16.0);
                }

                if fs_direction {
                    col += 1;
                    if col >= cols {
                        break;
                    }
                } else {
                    if col == 0 {
                        break;
                    }
                    col -= 1;
                }
            }

            std::mem::swap(&mut thiserr, &mut nexterr);
            fs_direction = !fs_direction;
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }
    }

    /// Smooth shapes plus noise, with a translucent region, so that dithering, the dither
    /// map and the minimum-opacity handling all have something to do.
    fn pixels(width: usize, height: usize, seed: u64) -> Vec<RGBA> {
        let mut rng = Lcg(seed);
        let mut px = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let n = (rng.next() % 24) as usize;
                let c = |v: usize| (v % 256) as u8;
                let a = if x * 3 < width { c(150 + (y * 2 + n) % 106) } else { 255 };
                px.push(RGBA::new(c(x * 3 + n), c(y * 2 + n), c((x + y) + n / 2), a));
            }
        }
        px
    }

    fn palette(colors: usize, seed: u64) -> PalF {
        let lut = ColorLut::new(0.45455, ColorSpace::Rgb);
        let mut rng = Lcg(seed);
        let items = (0..colors)
            .map(|i| {
                let v = rng.next().to_le_bytes();
                let a = if i % 5 == 0 { v[3] | 0x80 } else { 255 };
                PalItem {
                    color: FPixel::from_rgba(&lut, RGBA::new(v[0], v[1], v[2], a)),
                    popularity: 1.0,
                    fixed: false,
                }
            })
            .collect();
        PalF { items, subset: None }
    }

    #[derive(Clone, Copy, Debug)]
    struct Case {
        width: usize,
        height: usize,
        colors: usize,
        min_opacity: u8,
        dither_map: bool,
        hints: bool,
        level: f32,
    }

    fn cases() -> Vec<Case> {
        let mut cases = Vec::new();
        for (width, height) in [(1, 150), (37, 64), (37, 65), (61, 200), (23, 300)] {
            for (colors, min_opacity, dither_map, hints, level) in [
                (40, 255, false, false, 1.0),
                (40, 200, true, true, 1.0),
                (12, 255, true, false, 0.5),
                (256, 230, false, true, 0.7),
            ] {
                cases.push(Case {
                    width,
                    height,
                    colors,
                    min_opacity,
                    dither_map,
                    hints,
                    level,
                });
            }
        }
        cases
    }

    /// Image, dither map and initial output (search hints) for rows `rows` of a case.
    fn setup(case: &Case, rows: std::ops::Range<usize>) -> (Image<'static>, Vec<u8>) {
        let mut attr = Attributes::new();
        attr.set_min_opacity(case.min_opacity).unwrap();
        let w = case.width;
        let all = pixels(w, case.height, 7);
        let mut image = attr
            .new_image(&all[rows.start * w..rows.end * w], w, rows.len(), 0.0)
            .unwrap();
        let mut rng = Lcg(11);
        let map: Vec<u8> = (0..w * case.height).map(|_| rng.next() as u8).collect();
        let hints: Vec<u8> = (0..w * case.height)
            .map(|_| (rng.next() as usize % case.colors) as u8)
            .collect();
        if case.dither_map {
            image.dither_map = Some(map[rows.start * w..rows.end * w].into());
        }
        (image, hints[rows.start * w..rows.end * w].to_vec())
    }

    fn run(case: &Case, profile: Profile) -> Vec<u8> {
        let (mut image, mut out) = setup(case, 0..case.height);
        let map = palette(case.colors, 3);
        let mut rows: Vec<&mut [u8]> = out.chunks_mut(case.width).collect();
        remap_to_palette_floyd(&mut image, &mut rows, &map, 0.02, true, case.hints, case.level, profile);
        out
    }

    fn run_reference(case: &Case, rows: std::ops::Range<usize>, seed: u32, profile: Profile) -> Vec<u8> {
        let (mut image, mut out) = setup(case, rows);
        let map = palette(case.colors, 3);
        let mut rows: Vec<&mut [u8]> = out.chunks_mut(case.width).collect();
        reference_floyd(
            &mut image, &mut rows, &map, 0.02, true, case.hints, case.level, seed, profile,
        );
        out
    }

    #[test]
    fn compat_is_the_original_pass() {
        for case in cases() {
            assert_eq!(
                run(&case, Profile::Compat),
                run_reference(&case, 0..case.height, NOISE_SEED, Profile::Compat),
                "{case:?}"
            );
        }
    }

    /// Each strip must equal the original pass run on its own rows plus the warm-up rows
    /// above (with that strip's seed), and nothing else.
    #[test]
    fn improved_strips_are_independent_original_passes() {
        let mut changed = 0;
        for case in cases() {
            let out = run(&case, Profile::Improved);
            if out != run(&case, Profile::Compat) {
                changed += 1;
            }
            let w = case.width;
            for (index, first_row) in (0..case.height).step_by(STRIP_ROWS).enumerate() {
                let end = (first_row + STRIP_ROWS).min(case.height);
                let start = first_row.saturating_sub(WARMUP_ROWS);
                let expected = run_reference(&case, start..end, NOISE_SEED + index as u32, Profile::Improved);
                assert_eq!(
                    out[first_row * w..end * w],
                    expected[(first_row - start) * w..],
                    "{case:?} strip {index}"
                );
            }
            if case.height <= STRIP_ROWS {
                assert_eq!(
                    out,
                    run_reference(&case, 0..case.height, NOISE_SEED, Profile::Improved),
                    "{case:?}"
                );
            }
        }
        // Taller cases must actually be split. (With a random dither map, the error state
        // converges within the warm-up rows, so some of them happen to match Compat.)
        assert!(changed >= 6, "{changed}");
    }

    #[cfg(feature = "threads")]
    #[test]
    fn improved_is_independent_of_thread_count() {
        let case = Case {
            width: 50,
            height: 700,
            colors: 64,
            min_opacity: 220,
            dither_map: true,
            hints: true,
            level: 1.0,
        };
        let with_threads = |threads| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            pool.install(|| run(&case, Profile::Improved))
        };
        let one = with_threads(1);
        assert_eq!(one, with_threads(2));
        assert_eq!(one, with_threads(5));
        assert_eq!(one, with_threads(16));
    }
}
