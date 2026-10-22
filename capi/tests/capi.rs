//! Tests of the C API, calling the exported `extern "C"` functions as a C program would.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use imgquant::*;
use libimgquant::{Profile, RGBA};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// A deterministic test image: gradients, noise, a translucent area and a transparent corner.
fn test_image(width: usize, height: usize) -> Vec<RGBA> {
    let mut seed = 0x1234_5678_u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let r = (x * 255 / width) as u8;
            let g = (y * 255 / height) as u8;
            let b = ((x + y) * 7 % 256) as u8 ^ (noise() >> 3);
            let a = if x < width / 6 && y < height / 6 {
                0
            } else if y > height * 3 / 4 {
                (128 + x * 127 / width) as u8
            } else {
                255
            };
            pixels.push(RGBA::new(r, g, b, a));
        }
    }
    pixels
}

fn as_bytes(pixels: &[RGBA]) -> Vec<u8> {
    pixels.iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect()
}

#[derive(Clone, Copy, Debug)]
struct Settings {
    profile: Profile,
    speed: i32,
    quality: (u8, u8),
    max_colors: u32,
    dither: f32,
    fixed: Option<RGBA>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            profile: Profile::Improved,
            speed: 3,
            quality: (0, 100),
            max_colors: 256,
            dither: 0.0,
            fixed: None,
        }
    }
}

struct Output {
    palette: Vec<RGBA>,
    indices: Vec<u8>,
    error: f64,
    quality: i32,
    error_before_remap: f64,
}

fn rust_flow(pixels: &[RGBA], width: usize, height: usize, s: Settings) -> Output {
    let mut attr = libimgquant::new();
    attr.set_profile(s.profile);
    attr.set_speed(s.speed).unwrap();
    attr.set_quality(s.quality.0, s.quality.1).unwrap();
    attr.set_max_colors(s.max_colors).unwrap();
    let mut image = attr.new_image(pixels.to_vec(), width, height, 0.0).unwrap();
    if let Some(c) = s.fixed {
        image.add_fixed_color(c).unwrap();
    }
    let mut result = attr.quantize(&mut image).unwrap_or_else(|e| panic!("{s:?}: {e}"));
    let error_before_remap = result.quantization_error().unwrap_or(-1.0);
    result.set_dithering_level(s.dither).unwrap();
    let (palette, indices) = result.remapped(&mut image).unwrap();
    Output {
        palette,
        indices,
        error: result.quantization_error().unwrap_or(-1.0),
        quality: result.quantization_quality().map_or(-1, i32::from),
        error_before_remap,
    }
}

fn palette_colors(palette: *const liq_palette) -> Vec<RGBA> {
    assert!(!palette.is_null());
    let palette = unsafe { &*palette };
    assert!(palette.count >= 1 && palette.count <= 256);
    palette.entries[..palette.count as usize]
        .iter()
        .map(|c| RGBA::new(c.r, c.g, c.b, c.a))
        .collect()
}

fn configured_attr(s: Settings) -> *mut liq_attr {
    unsafe {
        let attr = liq_attr_create();
        assert!(!attr.is_null());
        let profile = match s.profile {
            Profile::Compat => LIQ_PROFILE_COMPAT,
            _ => LIQ_PROFILE_IMPROVED,
        };
        assert_eq!(liq_set_profile(attr, profile), liq_error::LIQ_OK);
        assert_eq!(liq_set_speed(attr, s.speed), liq_error::LIQ_OK);
        assert_eq!(
            liq_set_quality(attr, s.quality.0.into(), s.quality.1.into()),
            liq_error::LIQ_OK
        );
        assert_eq!(liq_set_max_colors(attr, s.max_colors as c_int), liq_error::LIQ_OK);
        attr
    }
}

/// Quantizes and remaps `image` (consumed) with the C API.
unsafe fn c_quantize_remap(attr: *mut liq_attr, image: *mut liq_image, s: Settings) -> Output {
    unsafe {
        assert!(!image.is_null());
        let (w, h) = (liq_image_get_width(image), liq_image_get_height(image));
        if let Some(c) = s.fixed {
            let color = liq_color {
                r: c.r,
                g: c.g,
                b: c.b,
                a: c.a,
            };
            assert_eq!(liq_image_add_fixed_color(image, color), liq_error::LIQ_OK);
        }
        let result = liq_quantize_image(attr, image);
        assert!(!result.is_null());
        let error_before_remap = liq_get_quantization_error(result);
        assert_eq!(liq_set_dithering_level(result, s.dither), liq_error::LIQ_OK);
        let mut indices = vec![0u8; (w * h) as usize];
        assert_eq!(
            liq_write_remapped_image(result, image, indices.as_mut_ptr().cast(), indices.len()),
            liq_error::LIQ_OK
        );
        let out = Output {
            palette: palette_colors(liq_get_palette(result)),
            indices,
            error: liq_get_quantization_error(result),
            quality: liq_get_quantization_quality(result),
            error_before_remap,
        };
        liq_result_destroy(result);
        liq_image_destroy(image);
        out
    }
}

fn c_flow_bitmap(pixels: &[RGBA], width: usize, height: usize, s: Settings) -> Output {
    let mut bytes = as_bytes(pixels);
    unsafe {
        let attr = configured_attr(s);
        let image = liq_image_create_rgba(attr, bytes.as_mut_ptr().cast(), width as c_int, height as c_int, 0.0);
        let out = c_quantize_remap(attr, image, s);
        liq_attr_destroy(attr);
        out
    }
}

fn assert_same(c: &Output, rust: &Output, what: &str) {
    assert_eq!(c.palette, rust.palette, "{what}: palette");
    assert!(c.indices == rust.indices, "{what}: indices differ");
    assert_eq!(c.error.to_bits(), rust.error.to_bits(), "{what}: error");
    assert_eq!(c.quality, rust.quality, "{what}: quality");
    assert_eq!(
        c.error_before_remap.to_bits(),
        rust.error_before_remap.to_bits(),
        "{what}: error before remap"
    );
}

#[test]
fn output_matches_rust_api() {
    let (w, h) = (83, 61);
    let pixels = test_image(w, h);
    for profile in [Profile::Compat, Profile::Improved] {
        for speed in [1, 3, 6, 10] {
            for dither in [0.0, 1.0] {
                let s = Settings {
                    profile,
                    speed,
                    dither,
                    ..Settings::default()
                };
                let what = format!("{s:?}");
                let expected = rust_flow(&pixels, w, h, s);
                assert_same(&c_flow_bitmap(&pixels, w, h, s), &expected, &what);
            }
        }
        let s = Settings {
            profile,
            quality: (0, 70),
            max_colors: 64,
            dither: 0.5,
            fixed: Some(RGBA::new(1, 2, 3, 255)),
            ..Settings::default()
        };
        let expected = rust_flow(&pixels, w, h, s);
        let c = c_flow_bitmap(&pixels, w, h, s);
        assert!(c.palette.contains(&RGBA::new(1, 2, 3, 255)));
        assert!(c.palette.len() <= 64);
        assert_same(&c, &expected, &format!("{s:?}"));
    }
}

#[test]
fn error_is_unknown_until_remapping() {
    let (w, h) = (40, 30);
    let pixels = test_image(w, h);
    let s = Settings {
        profile: Profile::Compat,
        speed: 10,
        ..Settings::default()
    };
    let c = c_flow_bitmap(&pixels, w, h, s);
    assert_eq!(c.error_before_remap, -1.0);
    assert!(c.error >= 0.0);
    assert!((0..=100).contains(&c.quality));
}

unsafe extern "C" fn copy_row_from_vec(row_out: *mut liq_color, row: c_int, width: c_int, user_info: *mut c_void) {
    let (pixels, image_width) = unsafe { &*user_info.cast::<(Vec<RGBA>, usize)>() };
    let start = row as usize * image_width;
    for x in 0..width as usize {
        let p = pixels[start + x];
        unsafe {
            *row_out.add(x) = liq_color {
                r: p.r,
                g: p.g,
                b: p.b,
                a: p.a,
            };
        }
    }
}

#[test]
fn rows_and_custom_images_match_bitmap() {
    let (w, h) = (71, 47);
    let pixels = test_image(w, h);
    for profile in [Profile::Compat, Profile::Improved] {
        let s = Settings {
            profile,
            dither: 1.0,
            ..Settings::default()
        };
        let expected = c_flow_bitmap(&pixels, w, h, s);
        assert_same(&expected, &rust_flow(&pixels, w, h, s), "bitmap");

        // separate row allocations, given in order
        let mut rows: Vec<Vec<u8>> = pixels.chunks(w).map(as_bytes).collect();
        let mut row_ptrs: Vec<*mut c_void> = rows.iter_mut().map(|r| r.as_mut_ptr().cast()).collect();
        unsafe {
            let attr = configured_attr(s);
            let image = liq_image_create_rgba_rows(attr, row_ptrs.as_mut_ptr(), w as c_int, h as c_int, 0.0);
            assert_same(&c_quantize_remap(attr, image, s), &expected, "rows");
            liq_attr_destroy(attr);
        }

        // a row callback
        let mut user = (pixels.clone(), w);
        unsafe {
            let attr = configured_attr(s);
            let image = liq_image_create_custom(
                attr,
                Some(copy_row_from_vec),
                ptr::from_mut(&mut user).cast(),
                w as c_int,
                h as c_int,
                0.0,
            );
            assert_same(&c_quantize_remap(attr, image, s), &expected, "custom");
            liq_attr_destroy(attr);
        }
    }
}

#[test]
fn remap_rows_and_remap_into_input_bitmap() {
    let (w, h) = (64, 50);
    let pixels = test_image(w, h);
    let s = Settings {
        dither: 1.0,
        ..Settings::default()
    };
    let expected = rust_flow(&pixels, w, h, s);
    unsafe {
        // as in the libimagequant 2.4.1 README: the remapped image is written over the bitmap
        let mut bitmap = as_bytes(&pixels);
        let attr = configured_attr(s);
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        assert_eq!(liq_set_dithering_level(result, s.dither), liq_error::LIQ_OK);
        assert_eq!(
            liq_write_remapped_image(result, image, bitmap.as_mut_ptr().cast(), bitmap.len()),
            liq_error::LIQ_OK
        );
        assert!(bitmap[..w * h] == expected.indices[..]);
        assert_eq!(palette_colors(liq_get_palette(result)), expected.palette);
        liq_result_destroy(result);
        liq_image_destroy(image);

        // row by row, into rows with padding, in reverse memory order
        let stride = w + 13;
        let mut out = vec![0xAAu8; stride * h];
        let mut row_ptrs: Vec<*mut u8> = (0..h).map(|y| out.as_mut_ptr().add((h - 1 - y) * stride)).collect();
        let mut bitmap = as_bytes(&pixels);
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        assert_eq!(liq_set_dithering_level(result, s.dither), liq_error::LIQ_OK);
        assert_eq!(
            liq_write_remapped_image_rows(result, image, row_ptrs.as_mut_ptr()),
            liq_error::LIQ_OK
        );
        for y in 0..h {
            let row = &out[(h - 1 - y) * stride..][..stride];
            assert!(row[..w] == expected.indices[y * w..][..w], "row {y}");
            assert!(row[w..].iter().all(|&b| b == 0xAA), "padding of row {y} untouched");
        }
        assert_eq!(palette_colors(liq_get_palette(result)), expected.palette);
        liq_result_destroy(result);
        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

#[test]
fn null_and_wrong_handles() {
    use liq_error::*;
    let null_attr = ptr::null_mut::<liq_attr>();
    let null_image = ptr::null_mut::<liq_image>();
    let null_result = ptr::null_mut::<liq_result>();
    let (w, h) = (32, 24);
    let mut bitmap = as_bytes(&test_image(w, h));
    unsafe {
        let attr = liq_attr_create();
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        assert!(!image.is_null() && !result.is_null());
        // handles of the other types
        let attrs = [null_attr, image.cast(), result.cast()];
        let images = [null_image, attr.cast(), result.cast()];
        let results = [null_result, attr.cast(), image.cast()];

        for a in attrs {
            assert!(liq_attr_copy(a).is_null());
            assert_eq!(liq_set_max_colors(a, 10), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_max_colors(a), -1);
            assert_eq!(liq_set_speed(a, 5), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_speed(a), -1);
            assert_eq!(liq_set_min_opacity(a, 100), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_min_opacity(a), -1);
            assert_eq!(liq_set_min_posterization(a, 1), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_min_posterization(a), -1);
            assert_eq!(liq_set_quality(a, 10, 20), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_min_quality(a), -1);
            assert_eq!(liq_get_max_quality(a), -1);
            assert_eq!(liq_set_profile(a, LIQ_PROFILE_COMPAT), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_profile(a), -1);
            liq_set_last_index_transparent(a, 1);
            liq_set_log_callback(a, Some(record_log), ptr::null_mut());
            liq_set_log_flush_callback(a, Some(record_flush), ptr::null_mut());
            assert!(liq_image_create_rgba(a, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0).is_null());
            let mut rows = [bitmap.as_mut_ptr().cast::<c_void>()];
            assert!(liq_image_create_rgba_rows(a, rows.as_mut_ptr(), 1, 1, 0.0).is_null());
            assert!(liq_image_create_custom(a, Some(copy_row_from_vec), ptr::null_mut(), 1, 1, 0.0).is_null());
            assert!(liq_quantize_image(a, image).is_null());
            liq_attr_destroy(a);
        }
        for i in images {
            assert_eq!(liq_image_set_memory_ownership(i, LIQ_OWN_PIXELS), LIQ_INVALID_POINTER);
            assert_eq!(liq_image_add_fixed_color(i, liq_color::default()), LIQ_INVALID_POINTER);
            assert_eq!(liq_image_get_width(i), -1);
            assert_eq!(liq_image_get_height(i), -1);
            assert!(liq_quantize_image(attr, i).is_null());
            let mut buf = vec![0u8; w * h];
            assert_eq!(
                liq_write_remapped_image(result, i, buf.as_mut_ptr().cast(), buf.len()),
                LIQ_INVALID_POINTER
            );
            let mut rows = vec![buf.as_mut_ptr(); h];
            assert_eq!(
                liq_write_remapped_image_rows(result, i, rows.as_mut_ptr()),
                LIQ_INVALID_POINTER
            );
            liq_image_destroy(i);
        }
        for r in results {
            assert_eq!(liq_set_dithering_level(r, 0.5), LIQ_INVALID_POINTER);
            assert_eq!(liq_set_output_gamma(r, 0.5), LIQ_INVALID_POINTER);
            assert_eq!(liq_get_output_gamma(r), -1.0);
            assert!(liq_get_palette(r).is_null());
            let mut buf = vec![0u8; w * h];
            assert_eq!(
                liq_write_remapped_image(r, image, buf.as_mut_ptr().cast(), buf.len()),
                LIQ_INVALID_POINTER
            );
            let mut rows = vec![buf.as_mut_ptr(); h];
            assert_eq!(
                liq_write_remapped_image_rows(r, image, rows.as_mut_ptr()),
                LIQ_INVALID_POINTER
            );
            assert_eq!(liq_get_quantization_error(r), -1.0);
            assert_eq!(liq_get_quantization_quality(r), -1);
            liq_result_destroy(r);
        }

        // NULL output buffers and rows
        assert_eq!(
            liq_write_remapped_image(result, image, ptr::null_mut(), w * h),
            LIQ_INVALID_POINTER
        );
        assert_eq!(
            liq_write_remapped_image_rows(result, image, ptr::null_mut()),
            LIQ_INVALID_POINTER
        );
        let mut buf = vec![0u8; w * h];
        let mut rows: Vec<*mut u8> = (0..h).map(|y| buf.as_mut_ptr().add(y * w)).collect();
        rows[h - 1] = ptr::null_mut();
        assert_eq!(
            liq_write_remapped_image_rows(result, image, rows.as_mut_ptr()),
            LIQ_INVALID_POINTER
        );

        // the handles still work
        assert_eq!(liq_get_max_colors(attr), 256);
        assert_eq!(liq_image_get_width(image), w as c_int);
        assert!(!liq_get_palette(result).is_null());
        liq_result_destroy(result);
        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

#[test]
fn defaults_and_setters() {
    use liq_error::*;
    unsafe {
        let attr = liq_attr_create();
        assert_eq!(liq_get_max_colors(attr), 256);
        assert_eq!(liq_get_speed(attr), 3);
        assert_eq!(liq_get_min_opacity(attr), 255);
        assert_eq!(liq_get_min_posterization(attr), 0);
        assert_eq!(liq_get_min_quality(attr), 0);
        assert_eq!(liq_get_max_quality(attr), 100);
        assert_eq!(liq_get_profile(attr), LIQ_PROFILE_IMPROVED);

        for colors in [i32::MIN, -1, 0, 1, 257, i32::MAX] {
            assert_eq!(liq_set_max_colors(attr, colors), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_max_colors(attr, 2), LIQ_OK);
        assert_eq!(liq_get_max_colors(attr), 2);
        assert_eq!(liq_set_max_colors(attr, 256), LIQ_OK);

        for speed in [i32::MIN, 0, 11, i32::MAX] {
            assert_eq!(liq_set_speed(attr, speed), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_speed(attr, 10), LIQ_OK);
        assert_eq!(liq_get_speed(attr), 10);

        for min in [-1, 256, i32::MAX] {
            assert_eq!(liq_set_min_opacity(attr, min), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_min_opacity(attr, 200), LIQ_OK);
        assert_eq!(liq_get_min_opacity(attr), 200);
        assert_eq!(liq_set_min_opacity(attr, 255), LIQ_OK);

        for bits in [-1, 5, i32::MAX] {
            assert_eq!(liq_set_min_posterization(attr, bits), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_min_posterization(attr, 4), LIQ_OK);
        assert_eq!(liq_get_min_posterization(attr), 4);
        assert_eq!(liq_set_min_posterization(attr, 0), LIQ_OK);

        for (min, target) in [(-1, 50), (0, -1), (0, 101), (60, 50), (i32::MIN, i32::MAX)] {
            assert_eq!(liq_set_quality(attr, min, target), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_quality(attr, 60, 80), LIQ_OK);
        assert_eq!((liq_get_min_quality(attr), liq_get_max_quality(attr)), (60, 80));
        assert_eq!(liq_set_quality(attr, 0, 100), LIQ_OK);

        for profile in [-1, 2, i32::MAX] {
            assert_eq!(liq_set_profile(attr, profile), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_profile(attr, LIQ_PROFILE_COMPAT), LIQ_OK);
        assert_eq!(liq_get_profile(attr), LIQ_PROFILE_COMPAT);
        assert_eq!(liq_get_speed(attr), 10, "settings are kept");

        // copies are independent
        let copy = liq_attr_copy(attr);
        assert!(!copy.is_null() && copy != attr);
        assert_eq!(liq_get_profile(copy), LIQ_PROFILE_COMPAT);
        assert_eq!(liq_set_max_colors(copy, 16), LIQ_OK);
        assert_eq!(liq_get_max_colors(attr), 256);
        assert_eq!(liq_get_max_colors(copy), 16);
        liq_attr_destroy(copy);
        liq_attr_destroy(attr);
    }
}

#[test]
fn image_creation_errors() {
    let (w, h) = (16, 8);
    let mut bitmap = as_bytes(&test_image(w, h));
    let data = bitmap.as_mut_ptr().cast::<c_void>();
    unsafe {
        let attr = liq_attr_create();
        for (width, height) in [
            (0, 1),
            (1, 0),
            (-1, 5),
            (5, -1),
            (i32::MIN, 1),
            (65536, 65536),
            (i32::MAX, 2),
        ] {
            assert!(liq_image_create_rgba(attr, data, width, height, 0.0).is_null());
            let mut rows = [data];
            assert!(liq_image_create_rgba_rows(attr, rows.as_mut_ptr(), width, height, 0.0).is_null());
            assert!(
                liq_image_create_custom(attr, Some(copy_row_from_vec), ptr::null_mut(), width, height, 0.0).is_null()
            );
        }
        for gamma in [-0.1, 1.01, f64::NAN, f64::INFINITY] {
            assert!(liq_image_create_rgba(attr, data, w as c_int, h as c_int, gamma).is_null());
        }
        // gamma 1 is allowed, and 0 means sRGB
        for gamma in [0.0, 0.2, 1.0] {
            let image = liq_image_create_rgba(attr, data, w as c_int, h as c_int, gamma);
            assert!(!image.is_null());
            liq_image_destroy(image);
        }
        assert!(liq_image_create_rgba(attr, ptr::null_mut(), w as c_int, h as c_int, 0.0).is_null());
        assert!(liq_image_create_rgba_rows(attr, ptr::null_mut(), w as c_int, h as c_int, 0.0).is_null());
        let mut rows: Vec<*mut c_void> = (0..h).map(|y| data.cast::<u8>().add(y * w * 4).cast()).collect();
        rows[3] = ptr::null_mut();
        assert!(liq_image_create_rgba_rows(attr, rows.as_mut_ptr(), w as c_int, h as c_int, 0.0).is_null());
        assert!(liq_image_create_custom(attr, None, ptr::null_mut(), w as c_int, h as c_int, 0.0).is_null());

        // at most 256 fixed colors
        let image = liq_image_create_rgba(attr, data, w as c_int, h as c_int, 0.0);
        for i in 0..256 {
            let c = liq_color {
                r: i as u8,
                g: 0,
                b: 0,
                a: 255,
            };
            assert_eq!(liq_image_add_fixed_color(image, c), liq_error::LIQ_OK);
        }
        assert_eq!(
            liq_image_add_fixed_color(image, liq_color::default()),
            liq_error::LIQ_BUFFER_TOO_SMALL
        );
        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

#[test]
fn result_errors() {
    use liq_error::*;
    let (w, h) = (48, 32);
    let mut bitmap = as_bytes(&test_image(w, h));
    unsafe {
        let attr = liq_attr_create();
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);

        // the new value is validated (libimagequant 2.4.1 checked the previous one)
        for level in [-0.01, 1.01, f32::NAN] {
            assert_eq!(liq_set_dithering_level(result, level), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_dithering_level(result, 0.0), LIQ_OK);
        assert_eq!(liq_set_dithering_level(result, 1.0), LIQ_OK);

        assert!((liq_get_output_gamma(result) - 0.45455).abs() < 1e-9);
        for gamma in [0.0, 1.0, -1.0, f64::NAN] {
            assert_eq!(liq_set_output_gamma(result, gamma), LIQ_VALUE_OUT_OF_RANGE);
        }
        assert_eq!(liq_set_output_gamma(result, 0.5), LIQ_OK);
        assert_eq!(liq_get_output_gamma(result), 0.5);

        let mut out = vec![0u8; w * h];
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), w * h - 1),
            LIQ_BUFFER_TOO_SMALL
        );
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), 0),
            LIQ_BUFFER_TOO_SMALL
        );
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), w * h),
            LIQ_OK
        );
        liq_result_destroy(result);

        // quality too low: NULL
        assert_eq!(liq_set_max_colors(attr, 2), LIQ_OK);
        assert_eq!(liq_set_quality(attr, 99, 100), LIQ_OK);
        assert!(liq_quantize_image(attr, image).is_null());

        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

#[test]
fn last_index_transparent() {
    let (w, h) = (40, 40);
    let mut bitmap = as_bytes(&test_image(w, h));
    unsafe {
        let attr = liq_attr_create();
        liq_set_last_index_transparent(attr, 1);
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        let mut out = vec![0u8; w * h];
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), out.len()),
            liq_error::LIQ_OK
        );
        let palette = palette_colors(liq_get_palette(result));
        assert_eq!(palette.last().unwrap().a, 0);
        assert_eq!(out[0] as usize, palette.len() - 1, "the transparent corner");
        liq_result_destroy(result);
        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

#[test]
fn palette_pointer_stays_valid() {
    let (w, h) = (60, 45);
    let pixels = test_image(w, h);
    let mut bitmap = as_bytes(&pixels);
    let s = Settings::default();
    let expected = rust_flow(&pixels, w, h, s);
    unsafe {
        let attr = liq_attr_create();
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        let before = liq_get_palette(result);
        assert_eq!(liq_get_palette(result), before, "same pointer on every call");

        let mut out = vec![0u8; w * h];
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), out.len()),
            liq_error::LIQ_OK
        );
        // the old pointer shows the palette the remapped pixels refer to
        assert_eq!(palette_colors(before), expected.palette);
        assert_eq!(liq_get_palette(result), before);

        assert_eq!(liq_set_dithering_level(result, 1.0), liq_error::LIQ_OK);
        assert_eq!(liq_get_palette(result), before);
        assert_eq!(
            liq_write_remapped_image(result, image, out.as_mut_ptr().cast(), out.len()),
            liq_error::LIQ_OK
        );
        assert_eq!(liq_get_palette(result), before);
        let dithered = palette_colors(before);
        assert!(out.iter().all(|&i| usize::from(i) < dithered.len()));

        // a second result has its own palette
        let other = liq_quantize_image(attr, image);
        assert_ne!(liq_get_palette(other), before);
        liq_result_destroy(other);
        assert_eq!(palette_colors(before), dithered);

        liq_result_destroy(result);
        liq_image_destroy(image);
        liq_attr_destroy(attr);
    }
}

// --- callbacks ---

#[derive(Default)]
struct LogRecord {
    /// (attr pointer, message, liq_get_max_colors(attr) inside the callback)
    entries: Mutex<Vec<(usize, String, c_int)>>,
}

const FLUSH: &str = "<flush>";

unsafe extern "C" fn record_log(attr: *const liq_attr, message: *const c_char, user_info: *mut c_void) {
    if user_info.is_null() {
        return;
    }
    let record = unsafe { &*user_info.cast::<LogRecord>() };
    let message = unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned();
    let colors = unsafe { liq_get_max_colors(attr) };
    record.entries.lock().unwrap().push((attr as usize, message, colors));
}

unsafe extern "C" fn record_flush(attr: *const liq_attr, user_info: *mut c_void) {
    if user_info.is_null() {
        return;
    }
    let record = unsafe { &*user_info.cast::<LogRecord>() };
    let colors = unsafe { liq_get_max_colors(attr) };
    record
        .entries
        .lock()
        .unwrap()
        .push((attr as usize, FLUSH.to_owned(), colors));
}

fn take(record: &LogRecord) -> Vec<(usize, String, c_int)> {
    std::mem::take(&mut *record.entries.lock().unwrap())
}

#[test]
fn log_callbacks_receive_their_attr() {
    let (w, h) = (50, 40);
    let mut bitmap = as_bytes(&test_image(w, h));
    let record = LogRecord::default();
    let user = ptr::from_ref(&record).cast_mut().cast::<c_void>();
    unsafe {
        let attr = liq_attr_create();
        liq_set_log_callback(attr, Some(record_log), user);
        liq_set_log_flush_callback(attr, Some(record_flush), user);
        assert_eq!(liq_set_max_colors(attr, 200), liq_error::LIQ_OK);
        assert!(take(&record).is_empty());

        // errors detected by the C API and by the library
        assert!(liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), 0, 5, 0.0).is_null());
        assert!(liq_image_create_rgba(attr, ptr::null_mut(), 5, 5, 0.0).is_null());
        assert!(liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), 5, 5, 2.0).is_null());
        assert!(liq_quantize_image(attr, ptr::null_mut()).is_null());
        let messages: Vec<String> = take(&record)
            .into_iter()
            .map(|(a, m, colors)| {
                assert_eq!((a, colors), (attr as usize, 200));
                m
            })
            .collect();
        assert_eq!(
            messages,
            [
                "  error: width and height must be > 0",
                "  error: invalid bitmap pointer",
                "  error: gamma must be >= 0 and <= 1 (try 1/gamma instead)",
                "  error: invalid image pointer",
            ]
        );

        // progress messages
        let image = liq_image_create_rgba(attr, bitmap.as_mut_ptr().cast(), w as c_int, h as c_int, 0.0);
        let result = liq_quantize_image(attr, image);
        assert!(!result.is_null());
        let log = take(&record);
        assert!(log.iter().any(|(_, m, _)| m.contains("made histogram")));
        assert!(
            log.iter()
                .all(|(a, m, c)| *a == attr as usize && m != FLUSH && *c == 200)
        );
        liq_result_destroy(result);

        // a copy calls the same callbacks with itself
        let copy = liq_attr_copy(attr);
        assert_eq!(liq_set_max_colors(copy, 100), liq_error::LIQ_OK);
        assert!(take(&record).is_empty(), "copying does not log or flush");
        let result = liq_quantize_image(copy, image);
        assert!(!result.is_null());
        let log = take(&record);
        assert!(!log.is_empty());
        assert!(log.iter().all(|(a, _, c)| *a == copy as usize && *c == 100));
        liq_result_destroy(result);

        // destroying the original flushes it, and the copy keeps working
        liq_attr_destroy(attr);
        assert_eq!(take(&record), [(attr as usize, FLUSH.to_owned(), 200)]);
        assert!(liq_image_create_rgba(copy, bitmap.as_mut_ptr().cast(), -1, 5, 0.0).is_null());
        let log = take(&record);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, copy as usize);

        // replacing the log callback flushes first; removing the flush callback stops flushes
        liq_set_log_callback(copy, None, ptr::null_mut());
        assert_eq!(take(&record), [(copy as usize, FLUSH.to_owned(), 100)]);
        assert!(liq_image_create_rgba(copy, bitmap.as_mut_ptr().cast(), -1, 5, 0.0).is_null());
        assert!(take(&record).is_empty());
        liq_set_log_flush_callback(copy, None, ptr::null_mut());
        liq_set_log_callback(copy, Some(record_log), user);
        liq_attr_destroy(copy);
        assert!(take(&record).is_empty());
        liq_image_destroy(image);
    }
}

struct ConcurrencyProbe {
    pixels: Vec<RGBA>,
    width: usize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    calls: AtomicUsize,
    threads: Mutex<Vec<std::thread::ThreadId>>,
}

unsafe extern "C" fn probe_row(row_out: *mut liq_color, row: c_int, width: c_int, user_info: *mut c_void) {
    let probe = unsafe { &*user_info.cast::<ConcurrencyProbe>() };
    let now = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
    probe.max_active.fetch_max(now, Ordering::SeqCst);
    probe.calls.fetch_add(1, Ordering::SeqCst);
    {
        let mut threads = probe.threads.lock().unwrap();
        let id = std::thread::current().id();
        if !threads.contains(&id) {
            threads.push(id);
        }
    }
    let start = row as usize * probe.width;
    for x in 0..width as usize {
        let p = probe.pixels[start + x];
        unsafe {
            *row_out.add(x) = liq_color {
                r: p.r,
                g: p.g,
                b: p.b,
                a: p.a,
            };
        }
    }
    // stay in the callback long enough for overlaps to show up
    for _ in 0..2000 {
        std::hint::spin_loop();
    }
    probe.active.fetch_sub(1, Ordering::SeqCst);
}

#[test]
fn row_callbacks_never_run_concurrently() {
    let (w, h) = (320, 240);
    let pixels = test_image(w, h);
    let probe = ConcurrencyProbe {
        pixels: pixels.clone(),
        width: w,
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        threads: Mutex::new(Vec::new()),
    };
    let s = Settings {
        speed: 1,
        dither: 1.0,
        ..Settings::default()
    };
    unsafe {
        let attr = configured_attr(s);
        let image = liq_image_create_custom(
            attr,
            Some(probe_row),
            ptr::from_ref(&probe).cast_mut().cast(),
            w as c_int,
            h as c_int,
            0.0,
        );
        let out = c_quantize_remap(attr, image, s);
        liq_attr_destroy(attr);
        assert_same(&out, &rust_flow(&pixels, w, h, s), "custom image");
    }
    assert!(probe.calls.load(Ordering::SeqCst) >= h);
    assert_eq!(probe.max_active.load(Ordering::SeqCst), 1);
}

// --- memory ownership ---

static CUSTOM_MALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static CUSTOM_FREES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe extern "C" fn counting_malloc(size: usize) -> *mut c_void {
    CUSTOM_MALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
    unsafe { malloc(size) }
}

unsafe extern "C" fn counting_free(ptr: *mut c_void) {
    CUSTOM_FREES.lock().unwrap().push(ptr as usize);
    unsafe { free(ptr) };
}

fn free_counts() -> HashMap<usize, usize> {
    let mut counts = HashMap::new();
    for p in std::mem::take(&mut *CUSTOM_FREES.lock().unwrap()) {
        *counts.entry(p).or_default() += 1;
    }
    counts
}

/// Allocates a copy of `pixels` with C malloc.
fn malloc_bitmap(pixels: &[RGBA]) -> *mut c_void {
    let bytes = as_bytes(pixels);
    unsafe {
        let p = malloc(bytes.len());
        assert!(!p.is_null());
        ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        p
    }
}

/// Allocates a row pointer array with C malloc, pointing at the rows of `bitmap` in reverse
/// order (so that the first row is not the lowest address).
fn malloc_reversed_rows(bitmap: *mut c_void, width: usize, height: usize) -> *mut *mut c_void {
    unsafe {
        let rows = malloc(height * size_of::<*mut c_void>()).cast::<*mut c_void>();
        assert!(!rows.is_null());
        for y in 0..height {
            rows.add(y)
                .write(bitmap.cast::<u8>().add((height - 1 - y) * width * 4).cast());
        }
        rows
    }
}

#[test]
fn memory_ownership_with_custom_allocator() {
    use liq_error::*;
    let (w, h) = (24, 16);
    let pixels = test_image(w, h);
    // reversed rows, as `malloc_reversed_rows` makes them
    let reversed: Vec<RGBA> = pixels.chunks(w).rev().flatten().copied().collect();
    unsafe {
        assert!(liq_attr_create_with_allocator(Some(counting_malloc), None).is_null());
        assert!(liq_attr_create_with_allocator(None, Some(counting_free)).is_null());
        let attr = liq_attr_create_with_allocator(Some(counting_malloc), Some(counting_free));
        assert!(!attr.is_null());

        // bitmap owned by the image
        let bitmap = malloc_bitmap(&pixels);
        let image = liq_image_create_rgba(attr, bitmap, w as c_int, h as c_int, 0.0);
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_ROWS),
            LIQ_VALUE_OUT_OF_RANGE
        );
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_ROWS | LIQ_OWN_PIXELS),
            LIQ_VALUE_OUT_OF_RANGE
        );
        assert_eq!(liq_image_set_memory_ownership(image, 0), LIQ_VALUE_OUT_OF_RANGE);
        assert_eq!(liq_image_set_memory_ownership(image, 1), LIQ_VALUE_OUT_OF_RANGE);
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS | 16),
            LIQ_VALUE_OUT_OF_RANGE
        );
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS), LIQ_OK);
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS), LIQ_OK);
        let result = liq_quantize_image(attr, image);
        assert!(!result.is_null());
        liq_result_destroy(result);
        assert!(free_counts().is_empty(), "nothing freed before the image is destroyed");
        liq_image_destroy(image);
        assert_eq!(free_counts(), HashMap::from([(bitmap as usize, 1)]));

        // rows and pixels owned by the image; the pixels are the lowest row
        let bitmap = malloc_bitmap(&reversed);
        let rows = malloc_reversed_rows(bitmap, w, h);
        let copy = liq_attr_copy(attr);
        let image = liq_image_create_rgba_rows(copy, rows, w as c_int, h as c_int, 0.0);
        assert!(!image.is_null());
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_ROWS), LIQ_OK);
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS), LIQ_OK);
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_ROWS | LIQ_OWN_PIXELS),
            LIQ_OK
        );
        // the image outlives the attrs, and uses their free function
        liq_attr_destroy(copy);
        let s = Settings::default();
        let out = c_quantize_remap(attr, image, s);
        assert_same(&out, &rust_flow(&pixels, w, h, s), "owned rows");
        assert_eq!(free_counts(), HashMap::from([(bitmap as usize, 1), (rows as usize, 1)]));

        // only the rows
        let bitmap = malloc_bitmap(&reversed);
        let rows = malloc_reversed_rows(bitmap, w, h);
        let image = liq_image_create_rgba_rows(attr, rows, w as c_int, h as c_int, 0.0);
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_ROWS), LIQ_OK);
        liq_image_destroy(image);
        assert_eq!(free_counts(), HashMap::from([(rows as usize, 1)]));
        free(bitmap);

        // nothing owned
        let bitmap = malloc_bitmap(&pixels);
        let image = liq_image_create_rgba(attr, bitmap, w as c_int, h as c_int, 0.0);
        liq_image_destroy(image);
        assert!(free_counts().is_empty());
        free(bitmap);

        // custom images own no memory
        let mut user = (pixels.clone(), w);
        let image = liq_image_create_custom(
            attr,
            Some(copy_row_from_vec),
            ptr::from_mut(&mut user).cast(),
            w as c_int,
            h as c_int,
            0.0,
        );
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS),
            LIQ_VALUE_OUT_OF_RANGE
        );
        liq_image_destroy(image);

        liq_attr_destroy(attr);
        assert!(free_counts().is_empty(), "handles are not freed with the custom free");
        assert_eq!(
            CUSTOM_MALLOC_CALLS.load(Ordering::SeqCst),
            0,
            "custom malloc is never called"
        );
    }
}

#[test]
fn memory_ownership_with_c_free() {
    let (w, h) = (24, 16);
    let pixels = test_image(w, h);
    let reversed: Vec<RGBA> = pixels.chunks(w).rev().flatten().copied().collect();
    unsafe {
        let attr = liq_attr_create();
        let bitmap = malloc_bitmap(&pixels);
        let image = liq_image_create_rgba(attr, bitmap, w as c_int, h as c_int, 0.0);
        assert_eq!(liq_image_set_memory_ownership(image, LIQ_OWN_PIXELS), liq_error::LIQ_OK);
        liq_image_destroy(image);

        let bitmap = malloc_bitmap(&reversed);
        let rows = malloc_reversed_rows(bitmap, w, h);
        let image = liq_image_create_rgba_rows(attr, rows, w as c_int, h as c_int, 0.0);
        assert_eq!(
            liq_image_set_memory_ownership(image, LIQ_OWN_ROWS | LIQ_OWN_PIXELS),
            liq_error::LIQ_OK
        );
        let s = Settings::default();
        assert_same(
            &c_quantize_remap(attr, image, s),
            &rust_flow(&pixels, w, h, s),
            "owned rows",
        );
        liq_attr_destroy(attr);
    }
}

// --- version and header ---

/// The header with LF line endings, whatever line endings the checkout has.
static HEADER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| include_str!("../include/libimgquant.h").replace("\r\n", "\n"));

#[test]
fn version() {
    assert_eq!(liq_version(), 30000);
    assert_eq!(LIQ_VERSION, 30000);
    assert_eq!(LIQ_VERSION_STRING.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    let (major, minor, patch) = (
        env!("CARGO_PKG_VERSION_MAJOR").parse::<c_int>().unwrap(),
        env!("CARGO_PKG_VERSION_MINOR").parse::<c_int>().unwrap(),
        env!("CARGO_PKG_VERSION_PATCH").parse::<c_int>().unwrap(),
    );
    assert_eq!(liq_version(), major * 10000 + minor * 100 + patch);
    assert!(HEADER.contains("#define LIQ_VERSION 30000\n"));
    assert!(HEADER.contains(&format!(
        "#define LIQ_VERSION_STRING \"{}\"\n",
        env!("CARGO_PKG_VERSION")
    )));
}

#[test]
fn header_matches_the_exports() {
    // referencing every function proves that it exists with this name
    let exported: &[(&str, *const ())] = &[
        ("liq_attr_create", liq_attr_create as *const ()),
        (
            "liq_attr_create_with_allocator",
            liq_attr_create_with_allocator as *const (),
        ),
        ("liq_attr_copy", liq_attr_copy as *const ()),
        ("liq_attr_destroy", liq_attr_destroy as *const ()),
        ("liq_set_max_colors", liq_set_max_colors as *const ()),
        ("liq_get_max_colors", liq_get_max_colors as *const ()),
        ("liq_set_speed", liq_set_speed as *const ()),
        ("liq_get_speed", liq_get_speed as *const ()),
        ("liq_set_min_opacity", liq_set_min_opacity as *const ()),
        ("liq_get_min_opacity", liq_get_min_opacity as *const ()),
        ("liq_set_min_posterization", liq_set_min_posterization as *const ()),
        ("liq_get_min_posterization", liq_get_min_posterization as *const ()),
        ("liq_set_quality", liq_set_quality as *const ()),
        ("liq_get_min_quality", liq_get_min_quality as *const ()),
        ("liq_get_max_quality", liq_get_max_quality as *const ()),
        (
            "liq_set_last_index_transparent",
            liq_set_last_index_transparent as *const (),
        ),
        ("liq_set_log_callback", liq_set_log_callback as *const ()),
        ("liq_set_log_flush_callback", liq_set_log_flush_callback as *const ()),
        ("liq_image_create_rgba_rows", liq_image_create_rgba_rows as *const ()),
        ("liq_image_create_rgba", liq_image_create_rgba as *const ()),
        ("liq_image_create_custom", liq_image_create_custom as *const ()),
        (
            "liq_image_set_memory_ownership",
            liq_image_set_memory_ownership as *const (),
        ),
        ("liq_image_add_fixed_color", liq_image_add_fixed_color as *const ()),
        ("liq_image_get_width", liq_image_get_width as *const ()),
        ("liq_image_get_height", liq_image_get_height as *const ()),
        ("liq_image_destroy", liq_image_destroy as *const ()),
        ("liq_quantize_image", liq_quantize_image as *const ()),
        ("liq_set_dithering_level", liq_set_dithering_level as *const ()),
        ("liq_set_output_gamma", liq_set_output_gamma as *const ()),
        ("liq_get_output_gamma", liq_get_output_gamma as *const ()),
        ("liq_get_palette", liq_get_palette as *const ()),
        ("liq_write_remapped_image", liq_write_remapped_image as *const ()),
        (
            "liq_write_remapped_image_rows",
            liq_write_remapped_image_rows as *const (),
        ),
        ("liq_get_quantization_error", liq_get_quantization_error as *const ()),
        (
            "liq_get_quantization_quality",
            liq_get_quantization_quality as *const (),
        ),
        ("liq_result_destroy", liq_result_destroy as *const ()),
        ("liq_version", liq_version as *const ()),
        ("liq_set_profile", liq_set_profile as *const ()),
        ("liq_get_profile", liq_get_profile as *const ()),
    ];
    let mut declared: Vec<&str> = HEADER
        .lines()
        .filter(|l| l.starts_with("LIQ_EXPORT "))
        .map(|l| {
            let name_end = l.find('(').unwrap();
            let start = l[..name_end].rfind([' ', '*']).unwrap() + 1;
            &l[start..name_end]
        })
        .collect();
    declared.sort_unstable();
    let mut names: Vec<&str> = exported
        .iter()
        .map(|(n, p)| {
            assert!(!p.is_null());
            *n
        })
        .collect();
    names.sort_unstable();
    assert_eq!(declared, names);

    // constants and layouts
    for (decl, value) in [
        ("LIQ_OK = 0", liq_error::LIQ_OK as c_int),
        ("LIQ_QUALITY_TOO_LOW = 99", liq_error::LIQ_QUALITY_TOO_LOW as c_int),
        (
            "LIQ_VALUE_OUT_OF_RANGE = 100",
            liq_error::LIQ_VALUE_OUT_OF_RANGE as c_int,
        ),
        ("LIQ_OWN_ROWS=4", LIQ_OWN_ROWS),
        ("LIQ_OWN_PIXELS=8", LIQ_OWN_PIXELS),
        ("LIQ_PROFILE_COMPAT = 0", LIQ_PROFILE_COMPAT),
        ("LIQ_PROFILE_IMPROVED = 1", LIQ_PROFILE_IMPROVED),
    ] {
        assert!(HEADER.contains(decl), "{decl}");
        assert_eq!(decl.rsplit(['=', ' ']).next().unwrap().parse::<c_int>().unwrap(), value);
    }
    // the implicitly numbered errors follow LIQ_VALUE_OUT_OF_RANGE in this order
    let order = [
        "LIQ_OUT_OF_MEMORY,",
        "LIQ_NOT_READY,",
        "LIQ_BITMAP_NOT_AVAILABLE,",
        "LIQ_BUFFER_TOO_SMALL,",
        "LIQ_INVALID_POINTER\n",
    ];
    let positions: Vec<usize> = order.iter().map(|d| HEADER.find(d).unwrap()).collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]));
    assert!(HEADER.find("LIQ_VALUE_OUT_OF_RANGE = 100,").unwrap() < positions[0]);
    let values = [
        liq_error::LIQ_OUT_OF_MEMORY,
        liq_error::LIQ_NOT_READY,
        liq_error::LIQ_BITMAP_NOT_AVAILABLE,
        liq_error::LIQ_BUFFER_TOO_SMALL,
        liq_error::LIQ_INVALID_POINTER,
    ];
    for (i, v) in values.into_iter().enumerate() {
        assert_eq!(v as c_int, 101 + i as c_int);
    }
    assert_eq!(size_of::<liq_color>(), 4);
    assert_eq!(size_of::<liq_palette>(), 4 + 256 * 4);
}
