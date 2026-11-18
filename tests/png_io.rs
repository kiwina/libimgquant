//! `png_io`: decoding hostile and unusual PNGs, and keeping the ICC profile.
#![cfg(feature = "png")]

mod common;

use std::error::Error as _;

use common::photo_like;
use libimgquant::png_io::{self, PngError, PngMetadata};
use libimgquant::{Error, RGBA};

/// zlib-compresses `raw`, whose first byte must be 0, with the png crate: the image data of
/// a one-row gray image without filters is exactly that.
fn zlib(raw: &[u8]) -> Vec<u8> {
    assert_eq!(raw[0], 0);
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, raw.len() as u32 - 1, 1);
    encoder.set_compression(png::Compression::High);
    encoder.set_filter(png::Filter::NoFilter);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&raw[1..]).unwrap();
    writer.finish().unwrap();
    let mut idat = Vec::new();
    let mut rest = &file[8..];
    while rest.len() >= 12 {
        let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        if &rest[4..8] == b"IDAT" {
            idat.extend_from_slice(&rest[8..8 + len]);
        }
        rest = &rest[12 + len..];
    }
    idat
}

/// A PNG with the header of `info` and `idat` as its image data.
fn png_with_idat(info: png::Info<'_>, idat: &[u8]) -> Vec<u8> {
    let mut file = Vec::new();
    let mut writer = png::Encoder::with_info(&mut file, info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_chunk(png::chunk::IDAT, idat).unwrap();
    writer.finish().unwrap();
    file
}

fn info(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, interlaced: bool) -> png::Info<'static> {
    let mut info = png::Info::with_size(width, height);
    info.color_type = color;
    info.bit_depth = depth;
    info.interlaced = interlaced;
    info
}

/// An image encoded by the png crate, not interlaced.
fn encode(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(data).unwrap();
    writer.finish().unwrap();
    file
}

/// The same image Adam7-interlaced, which the png crate can't write: the rows of the seven
/// passes, each with filter type 0.
fn encode_interlaced(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    const PASSES: [(usize, usize, usize, usize); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let (w, h) = (width as usize, height as usize);
    let bpp = data.len() / (w * h);
    let mut raw = Vec::new();
    for (x0, y0, dx, dy) in PASSES {
        // a pass without columns has no rows either
        if x0 >= w {
            continue;
        }
        for y in (y0..h).step_by(dy) {
            raw.push(0);
            for x in (x0..w).step_by(dx) {
                raw.extend_from_slice(&data[(y * w + x) * bpp..][..bpp]);
            }
        }
    }
    png_with_idat(info(width, height, color, depth, true), &zlib(&raw))
}

/// The peak memory use of this process, on Linux.
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Decoding a file that declares far more pixels than it holds fails without touching memory
/// for the declared size. All tests in this file are small, so the process's peak memory
/// shows any large allocation.
fn assert_fails_small(file: &[u8]) -> PngError {
    let err = png_io::decode(file).expect_err("decoding must fail");
    if let Some(peak) = peak_rss_bytes() {
        assert!(peak < 200 << 20, "peak memory {peak} bytes");
    }
    err
}

#[test]
fn huge_declared_size_is_rejected_before_allocating() {
    // a stored zlib block with one byte: 12 bytes of image data
    let idat = [0x78, 0x01, 0x01, 0x01, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x01, 0x00, 0x01];
    for interlaced in [false, true] {
        let file = png_with_idat(
            info(65535, 65535, png::ColorType::Rgba, png::BitDepth::Eight, interlaced),
            &idat,
        );
        assert_eq!(file.len(), 69);
        // over i32::MAX pixels, as for Image
        assert!(matches!(
            assert_fails_small(&file),
            PngError::Quantize(Error::ValueOutOfRange)
        ));
    }
}

#[test]
fn truncated_large_image_fails_early() {
    let (width, height) = (12_000, 12_000);
    // three rows of the 12000x12000 RGBA image: 576 MB decoded
    let mut raw = vec![0u8; 3 * (1 + width as usize * 4)];
    let mut file = png_with_idat(
        info(width, height, png::ColorType::Rgba, png::BitDepth::Eight, false),
        &zlib(&raw),
    );
    assert!(matches!(assert_fails_small(&file), PngError::Decode(_)));
    file.truncate(file.len() - 20);
    assert!(matches!(assert_fails_small(&file), PngError::Decode(_)));

    // interlaced: the whole frame would be allocated, so the data is checked first
    raw.truncate(1 + 1500 * 4);
    let file = png_with_idat(
        info(width, height, png::ColorType::Rgba, png::BitDepth::Eight, true),
        &zlib(&raw),
    );
    let err = assert_fails_small(&file);
    assert!(matches!(err, PngError::Decode(_)), "{err}");
}

#[test]
fn interlaced_decodes_like_plain() {
    use png::BitDepth::{Eight, Sixteen};
    use png::ColorType::{Grayscale, GrayscaleAlpha, Rgb, Rgba};
    for (width, height) in [(13, 11), (3, 2), (1, 1)] {
        let n = (width * height) as usize;
        let bytes = |len: usize| (0..len).map(|i| (i * 37 % 251) as u8).collect::<Vec<_>>();
        for (color, depth, data) in [
            (Rgba, Eight, bytes(n * 4)),
            (Grayscale, Eight, bytes(n)),
            (GrayscaleAlpha, Sixteen, bytes(n * 4)),
            (Rgb, Sixteen, bytes(n * 6)),
        ] {
            let plain = png_io::decode(&encode(width, height, color, depth, &data)).unwrap();
            let interlaced = png_io::decode(&encode_interlaced(width, height, color, depth, &data)).unwrap();
            assert_eq!((interlaced.width, interlaced.height), (width as usize, height as usize));
            assert_eq!(interlaced.pixels, plain.pixels, "{width}x{height} {color:?} {depth:?}");
        }
    }

    // a flat image compresses about 1000:1, close to deflate's limit, and must still decode
    let data = vec![0u8; 1024 * 1024];
    let file = encode_interlaced(1024, 1024, Grayscale, Eight, &data);
    assert!(file.len() < data.len() / 500, "{} bytes", file.len());
    let decoded = png_io::decode(&file).unwrap();
    assert!(decoded.pixels.iter().all(|&p| p == RGBA::new(0, 0, 0, 255)));
}

#[test]
fn sixteen_bit_samples_keep_their_high_byte() {
    let rgba: Vec<u8> = [0x12ff_u16, 0x3400, 0xfe01, 0x8080, 0x0001, 0xffff, 0x7f7f, 0x0000]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    let decoded = png_io::decode(&encode(2, 1, png::ColorType::Rgba, png::BitDepth::Sixteen, &rgba)).unwrap();
    assert_eq!(
        decoded.pixels,
        [RGBA::new(0x12, 0x34, 0xfe, 0x80), RGBA::new(0x00, 0xff, 0x7f, 0x00)]
    );

    // 16-bit gray with a tRNS key: the exact 16-bit value is transparent
    let gray: Vec<u8> = [0x1234_u16, 0x1235].iter().flat_map(|v| v.to_be_bytes()).collect();
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, 2, 1);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Sixteen);
    encoder.set_trns(vec![0x12, 0x34]);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&gray).unwrap();
    writer.finish().unwrap();
    let decoded = png_io::decode(&file).unwrap();
    assert_eq!(
        decoded.pixels,
        [RGBA::new(0x12, 0x12, 0x12, 0), RGBA::new(0x12, 0x12, 0x12, 255)]
    );
}

#[test]
fn animated_png_reads_the_default_image() {
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, 2, 1);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_animated(2, 0).unwrap();
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
    writer.write_image_data(&[9; 6]).unwrap();
    writer.finish().unwrap();
    let decoded = png_io::decode(&file).unwrap();
    assert_eq!(decoded.pixels, [RGBA::new(1, 2, 3, 255), RGBA::new(4, 5, 6, 255)]);
}

#[test]
fn icc_profile_round_trip() {
    let (width, height) = (48, 32);
    let pixels: Vec<u8> = photo_like(width, height, 7)
        .iter()
        .flat_map(|c| [c.r, c.g, c.b, c.a])
        .collect();
    let profile = fake_profile(b"RGB ");
    let mut info = info(
        width as u32,
        height as u32,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        false,
    );
    info.icc_profile = Some(profile.clone().into());
    info.source_gamma = Some(png::ScaledFloat::from_scaled(45_455));
    let mut input = Vec::new();
    let mut writer = png::Encoder::with_info(&mut input, info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&pixels).unwrap();
    writer.finish().unwrap();

    let decoded = png_io::decode(&input).unwrap();
    assert_eq!(decoded.icc_profile.as_deref(), Some(&profile[..]));
    assert_eq!(
        decoded.metadata(),
        PngMetadata::default()
            .with_gamma(0.45455)
            .with_icc_profile(profile.clone())
    );

    let (output, _) = png_io::compress_png(&input, &libimgquant::new(), 1.0).unwrap();
    let output = png_io::decode(&output).unwrap();
    assert_eq!((output.icc_profile, output.gamma), (Some(profile), Some(0.45455)));

    // no iCCP in, none out
    let plain = encode(
        width as u32,
        height as u32,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &pixels,
    );
    let (output, _) = png_io::compress_png(&plain, &libimgquant::new(), 1.0).unwrap();
    assert_eq!(png_io::decode(&output).unwrap().icc_profile, None);
    assert!(!output.windows(4).any(|w| w == b"iCCP"));
}

/// 3000 bytes with the given color space at bytes 16..20, where ICC headers keep it.
fn fake_profile(color_space: &[u8; 4]) -> Vec<u8> {
    let mut profile: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
    profile[16..20].copy_from_slice(color_space);
    profile
}

#[test]
fn gray_icc_profile_is_not_written_to_the_palette_image() {
    // PNG allows only RGB profiles on palette (color) images
    let (width, height) = (32, 16);
    let pixels: Vec<u8> = (0..width * height).map(|i| (i * 5 % 256) as u8).collect();
    let mut info = info(width, height, png::ColorType::Grayscale, png::BitDepth::Eight, false);
    info.icc_profile = Some(fake_profile(b"GRAY").into());
    let mut input = Vec::new();
    let mut writer = png::Encoder::with_info(&mut input, info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&pixels).unwrap();
    writer.finish().unwrap();

    let decoded = png_io::decode(&input).unwrap();
    assert_eq!(decoded.icc_profile, Some(fake_profile(b"GRAY")));
    let (output, _) = png_io::compress_png(&input, &libimgquant::new(), 1.0).unwrap();
    assert!(!output.windows(4).any(|w| w == b"iCCP"));
}

#[test]
fn error_source_is_not_repeated() {
    let err = png_io::decode(b"not a png").unwrap_err();
    let inner = match &err {
        PngError::Decode(e) => e.to_string(),
        _ => panic!("expected a decode error"),
    };
    // reporters print Display, then each source: the inner error is already in Display
    assert!(err.to_string().contains(&inner));
    assert!(err.source().is_none_or(|s| s.to_string() != inner));
    assert!(PngError::Quantize(Error::QualityTooLow).source().is_none());
}
