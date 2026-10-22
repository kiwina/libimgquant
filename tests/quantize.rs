mod common;

use common::{gradient, palette_pattern, photo_like, quantize_and_remap};
use libimgquant::{Error, RGBA};

#[test]
fn respects_max_colors() {
    for colors in [2, 3, 16, 17, 64, 255, 256] {
        let mut attr = libimgquant::new();
        attr.set_max_colors(colors).unwrap();
        let (palette, _) = quantize_and_remap(&attr, photo_like(120, 90, 1), 120, 90, 0.0);
        assert!(palette.len() <= colors as usize, "{} > {colors}", palette.len());
    }
}

#[test]
fn every_speed_produces_valid_output() {
    for speed in 1..=10 {
        let mut attr = libimgquant::new();
        attr.set_speed(speed).unwrap();
        attr.set_max_colors(32).unwrap();
        for dither in [0.0, 1.0] {
            quantize_and_remap(&attr, photo_like(97, 61, speed as u64), 97, 61, dither);
        }
    }
}

#[test]
fn few_colors_are_kept_exactly() {
    let colors = [
        RGBA::new(255, 0, 0, 255),
        RGBA::new(0, 255, 0, 255),
        RGBA::new(0, 0, 255, 255),
        RGBA::new(255, 255, 255, 255),
        RGBA::new(0, 0, 0, 255),
        RGBA::new(128, 64, 32, 255),
    ];
    let (w, h) = (40, 30);
    let pixels = palette_pattern(&colors, w, h);
    let attr = libimgquant::new();
    let (palette, indices) = quantize_and_remap(&attr, pixels.clone(), w, h, 1.0);
    assert_eq!(palette.len(), colors.len());
    for c in colors {
        assert!(palette.contains(&c), "{c:?} missing from {palette:?}");
    }
    for (p, &i) in pixels.iter().zip(&indices) {
        assert_eq!(*p, palette[usize::from(i)]);
    }
}

#[test]
fn single_color_image() {
    let (w, h) = (13, 7);
    let attr = libimgquant::new();
    let (palette, indices) = quantize_and_remap(&attr, vec![RGBA::new(10, 20, 30, 255); w * h], w, h, 1.0);
    assert_eq!(palette, [RGBA::new(10, 20, 30, 255)]);
    assert!(indices.iter().all(|&i| i == 0));
}

#[test]
fn fully_transparent_image() {
    let (w, h) = (16, 16);
    // "dirty" transparent pixels are all the same color
    let pixels: Vec<_> = (0..w * h).map(|i| RGBA::new(i as u8, 7, 9, 0)).collect();
    let attr = libimgquant::new();
    let (palette, _) = quantize_and_remap(&attr, pixels, w, h, 1.0);
    assert_eq!(palette.len(), 1);
    assert_eq!(palette[0].a, 0);
}

#[test]
fn transparent_colors_come_first() {
    let attr = libimgquant::new();
    let (palette, _) = quantize_and_remap(&attr, photo_like(100, 80, 3), 100, 80, 0.0);
    let first_opaque = palette.iter().position(|p| p.a == 255).expect("has opaque colors");
    assert!(first_opaque > 0, "image has translucent colors");
    assert!(palette[first_opaque..].iter().all(|p| p.a == 255), "{palette:?}");
}

#[test]
fn last_index_transparent() {
    let (w, h) = (32, 32);
    let mut pixels = gradient(w, h);
    for p in &mut pixels[..100] {
        *p = RGBA::new(0, 0, 0, 0);
    }
    let mut attr = libimgquant::new();
    attr.set_max_colors(16).unwrap();
    attr.set_last_index_transparent(true);
    let (palette, _) = quantize_and_remap(&attr, pixels, w, h, 0.0);
    assert_eq!(palette.last().unwrap().a, 0, "{palette:?}");
    assert!(palette[..palette.len() - 1].iter().all(|p| p.a != 0));
}

#[test]
fn quality_too_low() {
    let mut attr = libimgquant::new();
    attr.set_max_colors(2).unwrap();
    attr.set_quality(95, 100).unwrap();
    let mut image = attr.new_image(photo_like(64, 64, 4), 64, 64, 0.0).unwrap();
    assert_eq!(attr.quantize(&mut image).err(), Some(Error::QualityTooLow));
}

#[test]
fn quality_target_reduces_colors() {
    let (w, h) = (64, 64);
    let mut low = libimgquant::new();
    low.set_quality(0, 30).unwrap();
    let mut high = libimgquant::new();
    high.set_quality(0, 100).unwrap();
    let (low_pal, _) = quantize_and_remap(&low, gradient(w, h), w, h, 0.0);
    let (high_pal, _) = quantize_and_remap(&high, gradient(w, h), w, h, 0.0);
    assert!(
        low_pal.len() < high_pal.len(),
        "{} vs {}",
        low_pal.len(),
        high_pal.len()
    );
}

#[test]
fn quality_is_reported() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(gradient(80, 60), 80, 60, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();
    // speed 3 runs the feedback loop, which measures the error
    let q = res.quantization_quality().expect("measured during quantization");
    assert!((40..=100).contains(&q), "{q}");
    let e = res.quantization_error().unwrap();
    assert!(e >= 0.0);
    res.remapped(&mut image).unwrap();
    assert_eq!(res.quantization_quality(), Some(q));
}

#[test]
fn quality_is_known_after_remap_at_fastest_speed() {
    for profile in [libimgquant::Profile::Compat, libimgquant::Profile::Improved] {
        let mut attr = libimgquant::new();
        attr.set_profile(profile);
        attr.set_speed(10).unwrap();
        let mut image = attr.new_image(photo_like(80, 60, 6), 80, 60, 0.0).unwrap();
        let mut res = attr.quantize(&mut image).unwrap();
        // 2.4.1 skips error measurement at speed 10; Improved always runs one Lloyd step
        assert_eq!(
            res.quantization_quality().is_some(),
            profile == libimgquant::Profile::Improved
        );
        res.remapped(&mut image).unwrap();
        assert!(res.quantization_quality().is_some());
    }
}

#[test]
fn fixed_colors_are_in_palette() {
    let fixed = [
        RGBA::new(255, 0, 255, 255),
        RGBA::new(1, 2, 3, 255),
        RGBA::new(0, 0, 0, 0),
    ];
    let mut attr = libimgquant::new();
    attr.set_max_colors(8).unwrap();
    let mut image = attr.new_image(gradient(50, 50), 50, 50, 0.0).unwrap();
    for f in fixed {
        image.add_fixed_color(f).unwrap();
    }
    let mut res = attr.quantize(&mut image).unwrap();
    let (palette, _) = res.remapped(&mut image).unwrap();
    assert!(palette.len() <= 8);
    for f in fixed {
        let found = palette.iter().any(|p| {
            if f.a == 0 {
                p.a == 0
            } else {
                p.r.abs_diff(f.r) <= 1 && p.g.abs_diff(f.g) <= 1 && p.b.abs_diff(f.b) <= 1 && p.a == f.a
            }
        });
        assert!(found, "{f:?} not in {palette:?}");
    }
}

#[test]
fn fixed_colors_alone_fill_palette() {
    let mut attr = libimgquant::new();
    attr.set_max_colors(4).unwrap();
    let mut image = attr.new_image(gradient(20, 20), 20, 20, 0.0).unwrap();
    for i in 0..6 {
        image.add_fixed_color(RGBA::new(i * 40, 0, 0, 255)).unwrap();
    }
    let mut res = attr.quantize(&mut image).unwrap();
    let (palette, indices) = res.remapped(&mut image).unwrap();
    assert_eq!(palette.len(), 4);
    assert!(indices.iter().all(|&i| i < 4));
}

#[test]
fn too_many_fixed_colors() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(gradient(4, 4), 4, 4, 0.0).unwrap();
    for i in 0..256 {
        image.add_fixed_color(RGBA::new(i as u8, 0, 0, 255)).unwrap();
    }
    assert_eq!(
        image.add_fixed_color(RGBA::new(0, 0, 0, 255)),
        Err(Error::BufferTooSmall)
    );
}

#[test]
fn posterization_rounds_palette() {
    for bits in 1..=4u8 {
        let mut attr = libimgquant::new();
        attr.set_min_posterization(bits).unwrap();
        let (palette, _) = quantize_and_remap(&attr, photo_like(60, 60, 7), 60, 60, 0.0);
        let ok = |v: u8| v == ((v & !((1u8 << bits) - 1)) | (v >> (8 - bits)));
        for p in palette.iter().filter(|p| p.a > 0) {
            assert!(
                ok(p.r) && ok(p.g) && ok(p.b) && ok(p.a),
                "{p:?} not posterized to {bits} bits"
            );
        }
    }
}

#[test]
fn min_opacity_makes_almost_opaque_colors_opaque() {
    let (w, h) = (24, 24);
    let pixels: Vec<_> = gradient(w, h).into_iter().map(|p| RGBA { a: 250, ..p }).collect();
    let mut attr = libimgquant::new();
    attr.set_min_opacity(230).unwrap();
    let (palette, _) = quantize_and_remap(&attr, pixels, w, h, 0.0);
    assert!(palette.iter().all(|p| p.a == 255), "{palette:?}");
}

#[test]
fn output_gamma_changes_palette() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(gradient(40, 40), 40, 40, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();
    let default_gamma = res.palette_vec();
    assert!((res.output_gamma() - 0.45455).abs() < 1e-9);
    res.set_output_gamma(0.8).unwrap();
    let other_gamma = res.palette_vec();
    assert_ne!(default_gamma, other_gamma);
    assert_eq!(res.output_gamma(), 0.8);
}

#[test]
fn tiny_images() {
    for (w, h) in [(1, 1), (1, 7), (7, 1), (2, 2), (3, 3), (4, 4), (5, 4), (8, 3)] {
        for speed in [1, 3, 10] {
            let mut attr = libimgquant::new();
            attr.set_speed(speed).unwrap();
            for dither in [0.0, 0.5, 1.0] {
                quantize_and_remap(&attr, photo_like(w, h, (w * h) as u64), w, h, dither);
            }
        }
    }
}

#[test]
fn remapping_twice_and_another_image() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(photo_like(70, 50, 8), 70, 50, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();
    let first = res.remapped(&mut image).unwrap();
    let second = res.remapped(&mut image).unwrap();
    assert_eq!(first, second);

    res.set_dithering_level(1.0).unwrap();
    let dithered = res.remapped(&mut image).unwrap();
    assert_eq!(first.0.len(), dithered.0.len());

    let mut other = attr.new_image(photo_like(33, 91, 9), 33, 91, 0.0).unwrap();
    let (palette, indices) = res.remapped(&mut other).unwrap();
    assert_eq!(indices.len(), 33 * 91);
    assert!(indices.iter().all(|&i| usize::from(i) < palette.len()));
}

#[test]
fn deterministic() {
    let run = || {
        let mut attr = libimgquant::new();
        attr.set_speed(1).unwrap();
        quantize_and_remap(&attr, photo_like(150, 100, 10), 150, 100, 1.0)
    };
    assert_eq!(run(), run());
}

#[test]
#[cfg(feature = "threads")]
fn same_result_with_any_thread_count() {
    // large enough for chunked histogram iteration and remapping
    let (w, h) = (700, 400);
    let pixels = photo_like(w, h, 11);
    let run = |threads| {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        pool.install(|| {
            let attr = libimgquant::new();
            quantize_and_remap(&attr, pixels.clone(), w, h, 1.0)
        })
    };
    let one = run(1);
    assert_eq!(one, run(3));
    assert_eq!(one, run(8));
}
