//! One palette for several images with `Histogram`.

mod common;

use common::{Lcg, gradient, photo_like};
use libimgquant::{Attributes, Error, Histogram, HistogramEntry, Image, Profile, RGBA};

const PROFILES: [Profile; 2] = [Profile::Compat, Profile::Improved];

fn attr_for(profile: Profile, speed: i32) -> Attributes {
    let mut attr = libimgquant::new();
    attr.set_profile(profile);
    attr.set_speed(speed).unwrap();
    attr
}

/// Mean square error of the remapped image per pixel, of premultiplied colors and alpha in
/// 8-bit units (so color errors count less where pixels are more transparent).
fn mse(pixels: &[RGBA], palette: &[RGBA], indices: &[u8]) -> f64 {
    let premultiplied = |c: RGBA| {
        let a = f64::from(c.a) / 255.0;
        [
            f64::from(c.r) * a,
            f64::from(c.g) * a,
            f64::from(c.b) * a,
            f64::from(c.a),
        ]
    };
    let sum: f64 = pixels
        .iter()
        .zip(indices)
        .map(|(&p, &i)| {
            let (p, q) = (premultiplied(p), premultiplied(palette[usize::from(i)]));
            p.iter().zip(q).map(|(a, b)| (a - b).powi(2)).sum::<f64>()
        })
        .sum();
    sum / pixels.len() as f64
}

/// Animation frames: a photo-like image scrolling to the right. Only the last frame has a
/// magenta block (5% of its area).
fn frames(width: usize, height: usize, count: usize) -> Vec<Vec<RGBA>> {
    let base = photo_like(width + 8 * count, height, 3);
    (0..count)
        .map(|t| {
            let mut px: Vec<RGBA> = (0..height)
                .flat_map(|y| base[y * (width + 8 * count) + 8 * t..][..width].to_vec())
                .collect();
            if t == count - 1 {
                for y in 0..height / 4 {
                    for x in 0..width / 5 {
                        px[y * width + x] = RGBA::new(250, 0, 250, 255);
                    }
                }
            }
            px
        })
        .collect()
}

fn shared_palette(
    attr: &Attributes,
    frames: &[Vec<RGBA>],
    width: usize,
    height: usize,
    dither: f32,
) -> Vec<(Vec<RGBA>, Vec<u8>)> {
    let mut images: Vec<Image<'_>> = frames
        .iter()
        .map(|f| attr.new_image_borrowed(f, width, height, 0.0).unwrap())
        .collect();
    let mut histogram = Histogram::new(attr);
    for image in &mut images {
        histogram.add_image(attr, image).unwrap();
    }
    let mut result = histogram.quantize(attr).unwrap();
    result.set_dithering_level(dither).unwrap();
    let before = result.palette_vec();
    let out: Vec<_> = images.iter_mut().map(|i| result.remapped(i).unwrap()).collect();
    for (palette, indices) in &out {
        assert_eq!(*palette, before, "every frame has the same palette");
        assert!(indices.iter().all(|&i| usize::from(i) < palette.len()));
    }
    out
}

#[test]
fn one_image_is_as_good_as_quantize() {
    // Not bit-identical: the histogram keeps colors sorted (not in 2.4.1's hash order, which
    // decides ties in median cut), sums weights exactly, and remapping doesn't move or
    // reorder the palette. The quality is the same within a small margin.
    let (w, h) = (160, 120);
    for profile in PROFILES {
        for speed in [1, 3, 10] {
            for seed in [1, 2].into_iter().take(if speed == 3 { 2 } else { 1 }) {
                let pixels = photo_like(w, h, seed);
                let attr = attr_for(profile, speed);

                let mut image = attr.new_image_borrowed(&pixels, w, h, 0.0).unwrap();
                let (palette, indices) = attr.quantize(&mut image).unwrap().remapped(&mut image).unwrap();
                let single = mse(&pixels, &palette, &indices);

                let mut image = attr.new_image_borrowed(&pixels, w, h, 0.0).unwrap();
                let mut histogram = Histogram::new(&attr);
                histogram.add_image(&attr, &mut image).unwrap();
                let (palette, indices) = histogram.quantize(&attr).unwrap().remapped(&mut image).unwrap();
                let shared = mse(&pixels, &palette, &indices);

                // within 1% in practice
                assert!(
                    shared <= single * 1.03,
                    "{profile:?} speed {speed} seed {seed}: histogram MSE {shared:.3}, quantize MSE {single:.3}"
                );
            }
        }
    }
}

#[test]
fn colors_of_one_frame_get_palette_entries() {
    let (w, h) = (80, 60);
    let frames = frames(w, h, 4);
    for profile in PROFILES {
        let mut attr = attr_for(profile, 3);
        attr.set_max_colors(32).unwrap();
        let out = shared_palette(&attr, &frames, w, h, 0.0);
        let (palette, indices) = &out[3];
        // the magenta block, in the last frame only, is remapped to a close color
        let worst = (0..h / 4)
            .flat_map(|y| (0..w / 5).map(move |x| y * w + x))
            .map(|i| {
                let c = palette[usize::from(indices[i])];
                (i32::from(c.r) - 250).abs() + i32::from(c.g) + (i32::from(c.b) - 250).abs()
            })
            .max()
            .unwrap();
        assert!(worst <= 12, "{profile:?}: magenta off by {worst}");
        // the other frames lose little against palettes of their own (at most 1.2x in practice)
        for (f, (palette, indices)) in frames.iter().zip(&out).take(3) {
            let mut image = attr.new_image_borrowed(f, w, h, 0.0).unwrap();
            let (own_palette, own_indices) = attr.quantize(&mut image).unwrap().remapped(&mut image).unwrap();
            let (shared, own) = (mse(f, palette, indices), mse(f, &own_palette, &own_indices));
            assert!(
                shared < own * 1.5,
                "{profile:?}: shared palette MSE {shared:.2}, own palette {own:.2}"
            );
        }
    }
}

#[test]
fn dithered_frames_share_the_palette() {
    let (w, h) = (64, 48);
    let frames = frames(w, h, 3);
    // speeds 1-5 use a dither map, made by a first remapping that moves an unshared palette
    for profile in PROFILES {
        for speed in [1, 5, 8] {
            shared_palette(&attr_for(profile, speed), &frames, w, h, 1.0);
        }
    }
}

#[test]
fn gamma_must_match() {
    let attr = libimgquant::new();
    let pixels = gradient(16, 16);
    let mut srgb = attr.new_image_borrowed(&pixels, 16, 16, 0.0).unwrap();
    let mut other = attr.new_image_borrowed(&pixels, 16, 16, 0.5).unwrap();

    let mut histogram = Histogram::new(&attr);
    histogram.add_image(&attr, &mut srgb).unwrap();
    assert_eq!(histogram.add_image(&attr, &mut other), Err(Error::ValueOutOfRange));
    let entry = HistogramEntry {
        color: RGBA::new(1, 2, 3, 255),
        count: 1,
    };
    assert_eq!(histogram.add_colors(&[entry], 0.5), Err(Error::ValueOutOfRange));
    assert_eq!(
        histogram.add_fixed_color(RGBA::new(0, 0, 0, 255), 0.5),
        Err(Error::ValueOutOfRange)
    );
    // 0 is sRGB, the same as 0.45455
    histogram.add_colors(&[entry], 0.45455).unwrap();
    histogram.add_fixed_color(RGBA::new(0, 0, 0, 255), 0.0).unwrap();
    assert!(histogram.quantize(&attr).is_ok());

    // gamma outside 0-1
    let mut empty = Histogram::new(&attr);
    assert_eq!(empty.add_colors(&[entry], 1.5), Err(Error::ValueOutOfRange));
    assert_eq!(empty.add_colors(&[entry], -0.1), Err(Error::ValueOutOfRange));
    assert_eq!(empty.add_colors(&[entry], f64::NAN), Err(Error::ValueOutOfRange));
    // the first gamma decides
    empty.add_image(&attr, &mut other).unwrap();
    assert_eq!(empty.add_image(&attr, &mut srgb), Err(Error::ValueOutOfRange));
    let mut result = empty.quantize(&attr).unwrap();
    assert_eq!(result.output_gamma(), 0.5);
    result.remapped(&mut other).unwrap();
}

#[test]
fn profile_must_match_and_empty_fails() {
    let attr = attr_for(Profile::Improved, 3);
    let compat = attr_for(Profile::Compat, 3);
    let pixels = gradient(16, 16);
    let mut image = attr.new_image_borrowed(&pixels, 16, 16, 0.0).unwrap();
    let mut histogram = Histogram::new(&attr);
    assert_eq!(histogram.quantize(&attr).err(), Some(Error::ValueOutOfRange));
    assert_eq!(histogram.add_image(&compat, &mut image), Err(Error::ValueOutOfRange));
    histogram.add_image(&attr, &mut image).unwrap();
    assert_eq!(histogram.quantize(&compat).err(), Some(Error::ValueOutOfRange));
    assert!(histogram.quantize(&attr).is_ok());
}

#[test]
fn colors_from_entries() {
    // the 6x6x6 web palette, more pixels of the darker colors
    let entries: Vec<HistogramEntry> = (0..216u32)
        .map(|i| HistogramEntry {
            color: RGBA::new((i % 6 * 51) as u8, (i / 6 % 6 * 51) as u8, (i / 36 * 51) as u8, 255),
            count: 1000 - 4 * i,
        })
        .collect();
    for profile in PROFILES {
        let attr = attr_for(profile, 3);
        let mut histogram = Histogram::new(&attr);
        histogram.add_colors(&entries, 0.0).unwrap();
        // zero counts and repeated colors are fine
        histogram
            .add_colors(&[HistogramEntry { count: 0, ..entries[5] }, entries[5]], 0.0)
            .unwrap();
        // few colors are kept exactly
        let mut result = histogram.quantize(&attr).unwrap();
        let mut palette = result.palette_vec();
        let mut expected: Vec<RGBA> = entries.iter().map(|e| e.color).collect();
        palette.sort_by_key(|c| (c.r, c.g, c.b));
        expected.sort_by_key(|c| (c.r, c.g, c.b));
        assert_eq!(palette, expected, "{profile:?}");

        let mut few = attr.clone();
        few.set_max_colors(20).unwrap();
        let mut result = histogram.quantize(&few).unwrap();
        assert_eq!(result.palette().len(), 20);
        // an image of those colors remaps to the palette
        let pixels: Vec<RGBA> = entries.iter().map(|e| e.color).collect();
        let mut image = few.new_image(pixels, 18, 12, 0.0).unwrap();
        let (palette, indices) = result.remapped(&mut image).unwrap();
        assert!(indices.iter().all(|&i| usize::from(i) < palette.len()));
    }
}

#[test]
fn fixed_colors_are_in_the_palette() {
    let (w, h) = (48, 32);
    let pixels = photo_like(w, h, 9);
    let brand = RGBA::new(0, 128, 255, 255);
    let transparent = RGBA::new(0, 0, 0, 0);
    let image_fixed = RGBA::new(255, 255, 0, 255);
    for profile in PROFILES {
        let mut attr = attr_for(profile, 3);
        attr.set_max_colors(16).unwrap();
        let mut histogram = Histogram::new(&attr);
        // fixed colors of images are added too, once
        for _ in 0..2 {
            let mut image = attr.new_image_borrowed(&pixels, w, h, 0.0).unwrap();
            image.add_fixed_color(image_fixed).unwrap();
            histogram.add_image(&attr, &mut image).unwrap();
        }
        histogram.add_fixed_color(brand, 0.0).unwrap();
        histogram.add_fixed_color(brand, 0.0).unwrap();
        histogram.add_fixed_color(transparent, 0.0).unwrap();

        let mut result = histogram.quantize(&attr).unwrap();
        let palette = result.palette_vec();
        assert_eq!(palette.len(), 16);
        for fixed in [brand, image_fixed] {
            assert_eq!(palette.iter().filter(|&&c| c == fixed).count(), 1, "{profile:?}");
        }
        assert_eq!(palette.iter().filter(|c| c.a == 0).count(), 1, "{profile:?}");
    }

    let attr = libimgquant::new();
    let mut histogram = Histogram::new(&attr);
    for i in 0..=255u8 {
        histogram.add_fixed_color(RGBA::new(i, 0, 0, 255), 0.0).unwrap();
    }
    assert_eq!(
        histogram.add_fixed_color(RGBA::new(0, 1, 0, 255), 0.0),
        Err(Error::BufferTooSmall)
    );
    histogram.add_fixed_color(RGBA::new(7, 0, 0, 255), 0.0).unwrap();
}

#[test]
fn many_colors_are_posterized() {
    // speed 10 allows 2^17 histogram entries; four noise frames have about 2^18 colors
    let mut attr = attr_for(Profile::Improved, 10);
    attr.set_max_colors(64).unwrap();
    let (w, h) = (256, 256);
    let mut rng = Lcg::new(4);
    let frames: Vec<Vec<RGBA>> = (0..4)
        .map(|_| {
            (0..w * h)
                .map(|_| RGBA::new(rng.next_u8(), rng.next_u8(), rng.next_u8(), 255))
                .collect()
        })
        .collect();
    let out = shared_palette(&attr, &frames, w, h, 0.0);
    assert_eq!(out[0].0.len(), 64);
}

#[test]
fn same_result_every_time() {
    let (w, h) = (90, 70);
    let frames = frames(w, h, 3);
    for profile in PROFILES {
        let attr = attr_for(profile, 3);
        let first = shared_palette(&attr, &frames, w, h, 1.0);
        assert_eq!(first, shared_palette(&attr, &frames, w, h, 1.0));
    }
}

#[cfg(feature = "threads")]
#[test]
fn same_result_for_any_thread_count() {
    // several histogram bands (16384 pixels) and remapping chunks (65536 pixels)
    let (w, h) = (160, 480);
    let frames = frames(w, h, 3);
    for profile in PROFILES {
        let attr = attr_for(profile, 3);
        let run = |threads| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            pool.install(|| shared_palette(&attr, &frames, w, h, 1.0))
        };
        assert_eq!(run(1), run(5), "{profile:?}");
    }
}
