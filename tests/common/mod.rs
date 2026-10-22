#![allow(dead_code)]

use libimgquant::RGBA;

/// Small deterministic generator, so tests don't need a rand dependency.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u8(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 56) as u8
    }
}

/// Smooth opaque gradient with many distinct colors.
pub fn gradient(width: usize, height: usize) -> Vec<RGBA> {
    let mut px = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            px.push(RGBA::new(
                (x * 255 / width.max(2).saturating_sub(1)).min(255) as u8,
                (y * 255 / height.max(2).saturating_sub(1)).min(255) as u8,
                ((x + y) * 127 / (width + height)).min(255) as u8,
                255,
            ));
        }
    }
    px
}

/// Photo-like image: smooth shapes plus noise, with a semi-transparent region.
pub fn photo_like(width: usize, height: usize, seed: u64) -> Vec<RGBA> {
    let mut rng = Lcg::new(seed);
    let mut px = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let fx = x as f64 / width as f64;
            let fy = y as f64 / height as f64;
            let n = f64::from(rng.next_u8()) / 255.0 - 0.5;
            let r = 0.5 + 0.4 * (fx * 9.0).sin() * (fy * 4.0).cos() + 0.08 * n;
            let g = 0.5 + 0.4 * (fx * 3.0 + fy * 5.0).sin() + 0.05 * n;
            let b = 0.5 + 0.45 * ((fx - 0.5).hypot(fy - 0.5) * 14.0).cos();
            let a = if fx > 0.75 { 0.2 + 0.8 * fy } else { 1.0 };
            let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            px.push(RGBA::new(ch(r), ch(g), ch(b), ch(a)));
        }
    }
    px
}

/// Image made of the given colors in a repeating pattern.
pub fn palette_pattern(colors: &[RGBA], width: usize, height: usize) -> Vec<RGBA> {
    (0..width * height)
        .map(|i| colors[(i / 3 + i / width) % colors.len()])
        .collect()
}

/// Asserts the remapped output is valid, and returns the palette and indices.
pub fn quantize_and_remap(
    attr: &libimgquant::Attributes,
    pixels: Vec<RGBA>,
    width: usize,
    height: usize,
    dither: f32,
) -> (Vec<RGBA>, Vec<u8>) {
    let mut image = attr.new_image(pixels, width, height, 0.0).expect("image");
    let mut res = attr.quantize(&mut image).expect("quantize");
    res.set_dithering_level(dither).expect("dither level");
    let (palette, indices) = res.remapped(&mut image).expect("remap");
    assert!(!palette.is_empty());
    assert!(palette.len() <= attr.max_colors() as usize);
    assert_eq!(indices.len(), width * height);
    assert!(
        indices.iter().all(|&i| usize::from(i) < palette.len()),
        "index out of palette range"
    );
    (palette, indices)
}
