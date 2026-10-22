//! Freezes the output of the 2.4.1 algorithms (`Profile::Compat`).
//!
//! Any change to these hashes means the Compat profile no longer reproduces the
//! reviewed port of libimagequant 2.4.1. Regenerate only on purpose, with
//! `GOLDEN_PRINT=1 cargo test --test golden -- --nocapture` (and again with
//! `--no-default-features`, which has its own table).

mod common;

use common::{gradient, palette_pattern, photo_like};
use libimgquant::{Attributes, RGBA};

const FIXED: &[RGBA] = &[
    RGBA {
        r: 255,
        g: 0,
        b: 255,
        a: 255,
    },
    RGBA { r: 0, g: 0, b: 0, a: 0 },
];

struct Case {
    name: &'static str,
    pixels: Vec<RGBA>,
    width: usize,
    height: usize,
    setup: fn(&mut Attributes),
    dither: f32,
    fixed: &'static [RGBA],
    palette_first: bool,
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

fn run(case: &Case) -> u64 {
    let mut attr = libimgquant::new();
    attr.set_profile(libimgquant::Profile::Compat);
    (case.setup)(&mut attr);
    let mut image = attr
        .new_image(case.pixels.clone(), case.width, case.height, 0.0)
        .unwrap();
    for &f in case.fixed {
        image.add_fixed_color(f).unwrap();
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut res = match attr.quantize(&mut image) {
        Ok(res) => res,
        Err(e) => {
            fnv(&mut hash, format!("{e:?}").as_bytes());
            return hash;
        }
    };
    res.set_dithering_level(case.dither).unwrap();
    if case.palette_first {
        let early: Vec<u8> = res.palette().iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
        fnv(&mut hash, &early);
    }
    let (palette, indices) = res.remapped(&mut image).unwrap();
    fnv(&mut hash, &(palette.len() as u32).to_le_bytes());
    for c in &palette {
        fnv(&mut hash, &[c.r, c.g, c.b, c.a]);
    }
    fnv(&mut hash, &indices);
    let q = res.quantization_quality().unwrap_or(255);
    fnv(&mut hash, &[q]);
    fnv(
        &mut hash,
        &res.quantization_error().unwrap_or(-1.0).to_bits().to_le_bytes(),
    );
    hash
}

fn cases() -> Vec<Case> {
    let small = || photo_like(97, 61, 1);
    let base = |name, pixels: Vec<RGBA>, width, height, setup: fn(&mut Attributes), dither| Case {
        name,
        pixels,
        width,
        height,
        setup,
        dither,
        fixed: &[],
        palette_first: false,
    };
    let few = [
        RGBA::new(255, 0, 0, 255),
        RGBA::new(0, 255, 0, 128),
        RGBA::new(0, 0, 255, 255),
        RGBA::new(255, 255, 255, 0),
        RGBA::new(20, 30, 40, 255),
    ];
    vec![
        base("s1-d0", small(), 97, 61, |a| a.set_speed(1).unwrap(), 0.0),
        base("s1-d1", small(), 97, 61, |a| a.set_speed(1).unwrap(), 1.0),
        base("s3-d0", small(), 97, 61, |_| {}, 0.0),
        base("s3-d1", small(), 97, 61, |_| {}, 1.0),
        base("s8-d0", small(), 97, 61, |a| a.set_speed(8).unwrap(), 0.0),
        base("s8-d1", small(), 97, 61, |a| a.set_speed(8).unwrap(), 1.0),
        base("s10-d0", small(), 97, 61, |a| a.set_speed(10).unwrap(), 0.0),
        base("s10-d05", small(), 97, 61, |a| a.set_speed(10).unwrap(), 0.5),
        base(
            "grad-16c",
            gradient(64, 48),
            64,
            48,
            |a| a.set_max_colors(16).unwrap(),
            1.0,
        ),
        base("large-s3-d1", photo_like(400, 300, 7), 400, 300, |_| {}, 1.0),
        base(
            "large-s10-d0",
            photo_like(400, 300, 7),
            400,
            300,
            |a| a.set_speed(10).unwrap(),
            0.0,
        ),
        base(
            "opacity-lastidx",
            small(),
            97,
            61,
            |a| {
                a.set_speed(5).unwrap();
                a.set_min_opacity(200).unwrap();
                a.set_last_index_transparent(true);
            },
            1.0,
        ),
        base(
            "posterize2",
            small(),
            97,
            61,
            |a| a.set_min_posterization(2).unwrap(),
            0.0,
        ),
        base(
            "quality-30-60",
            gradient(80, 60),
            80,
            60,
            |a| a.set_quality(30, 60).unwrap(),
            1.0,
        ),
        base(
            "quality-too-low",
            small(),
            97,
            61,
            |a| {
                a.set_max_colors(4).unwrap();
                a.set_quality(90, 100).unwrap();
            },
            1.0,
        ),
        base("few-colors", palette_pattern(&few, 40, 30), 40, 30, |_| {}, 1.0),
        Case {
            fixed: FIXED,
            ..base(
                "fixed-8c",
                gradient(64, 48),
                64,
                48,
                |a| a.set_max_colors(8).unwrap(),
                1.0,
            )
        },
        Case {
            palette_first: true,
            ..base("palette-first", small(), 97, 61, |_| {}, 1.0)
        },
    ]
}

#[cfg(feature = "threads")]
const EXPECTED: &[(&str, u64)] = &[
    ("s1-d0", 0x0df63aae446383a4),
    ("s1-d1", 0xfb0a7e6c2973ff1d),
    ("s3-d0", 0x0798074b95b47dd9),
    ("s3-d1", 0xb227e6c634155933),
    ("s8-d0", 0xdc2cbdd6066105d7),
    ("s8-d1", 0xf36702e460bf2a81),
    ("s10-d0", 0xdc2cbdd6066105d7),
    ("s10-d05", 0x499e842226a4714e),
    ("grad-16c", 0xe2f9fed57fe69058),
    ("large-s3-d1", 0x6b046cd82681fd80),
    ("large-s10-d0", 0x03c71a6c044523e1),
    ("opacity-lastidx", 0x384b7dc9f921e1e0),
    ("posterize2", 0x391cb0793f3ec398),
    ("quality-30-60", 0x9ba297313a3502b3),
    ("quality-too-low", 0x65b4b3f64912686e),
    ("few-colors", 0xba36300f69428ea8),
    ("fixed-8c", 0x0c2f971158207ca1),
    ("palette-first", 0xdf1ec49c4e5b2463),
];

#[cfg(not(feature = "threads"))]
const EXPECTED: &[(&str, u64)] = &[
    ("s1-d0", 0x0df63aae446383a4),
    ("s1-d1", 0xfb0a7e6c2973ff1d),
    ("s3-d0", 0x0798074b95b47dd9),
    ("s3-d1", 0xb227e6c634155933),
    ("s8-d0", 0xdc2cbdd6066105d7),
    ("s8-d1", 0xf36702e460bf2a81),
    ("s10-d0", 0xdc2cbdd6066105d7),
    ("s10-d05", 0x499e842226a4714e),
    ("grad-16c", 0xe2f9fed57fe69058),
    ("large-s3-d1", 0x6b046cd82681fd80),
    ("large-s10-d0", 0x03c71a6c044523e1),
    ("opacity-lastidx", 0x384b7dc9f921e1e0),
    ("posterize2", 0x391cb0793f3ec398),
    ("quality-30-60", 0x9ba297313a3502b3),
    ("quality-too-low", 0x65b4b3f64912686e),
    ("few-colors", 0xba36300f69428ea8),
    ("fixed-8c", 0x0c2f971158207ca1),
    ("palette-first", 0xdf1ec49c4e5b2463),
];

#[test]
fn compat_profile_output_is_frozen() {
    let got: Vec<(&str, u64)> = cases().iter().map(|c| (c.name, run(c))).collect();
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        for (name, hash) in &got {
            println!("    (\"{name}\", 0x{hash:016x}),");
        }
        return;
    }
    assert_eq!(got.len(), EXPECTED.len(), "golden table out of date");
    for ((name, hash), (ename, ehash)) in got.iter().zip(EXPECTED) {
        assert_eq!(name, ename);
        assert_eq!(hash, ehash, "{name}: Compat output changed");
    }
}
