//! Reading any PNG and writing small palette PNGs, with the [png](https://crates.io/crates/png)
//! crate. Needs the `png` feature.
//!
//! [`compress_png`] does everything at once, like pngquant:
//!
//! ```
//! # fn main() -> Result<(), libimgquant::png_io::PngError> {
//! # let input = {
//! #     let mut out = Vec::new();
//! #     let mut encoder = png::Encoder::new(&mut out, 64, 64);
//! #     encoder.set_color(png::ColorType::Rgba);
//! #     let mut writer = encoder.write_header().unwrap();
//! #     let data: Vec<u8> = (0..64 * 64).flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 4) as u8, 99, 255]).collect();
//! #     writer.write_image_data(&data).unwrap();
//! #     writer.finish().unwrap();
//! #     out
//! # };
//! let mut attr = libimgquant::new();
//! attr.set_quality(65, 90)?;
//! // input: the bytes of a PNG file
//! let (output, quality) = libimgquant::png_io::compress_png(&input, &attr, 1.0)?;
//! assert!(quality.is_some_and(|q| q >= 65));
//! // output: a palette PNG with the input's dimensions and gamma
//! let decoded = libimgquant::png_io::decode(&output)?;
//! assert_eq!((decoded.width, decoded.height), (64, 64));
//! # Ok(())
//! # }
//! ```
//!
//! [`decode`] and [`encode_indexed`] are the two halves, for programs that set up the
//! quantization themselves.

use std::fmt;
use std::io::Cursor;

use crate::{Attributes, Error, RGBA};

/// A PNG decoded to 8-bit RGBA by [`decode`].
///
/// More fields (such as color space information) may be added in minor releases.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DecodedPng {
    /// `width * height` pixels, row by row.
    pub pixels: Vec<RGBA>,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// The gamma of the gAMA chunk (for example 0.45455), or `None` if there is none or an
    /// sRGB chunk overrides it. Pass it on to [`encode_indexed`] to keep it.
    pub gamma: Option<f64>,
}

impl DecodedPng {
    /// The gamma to create the image with ([`Attributes::new_image`]): [`gamma`](Self::gamma)
    /// when the quantizer accepts it (above 0, at most 1), otherwise 0 (sRGB).
    #[must_use]
    pub fn image_gamma(&self) -> f64 {
        self.gamma.filter(|&g| g > 0.0 && g <= 1.0).unwrap_or(0.0)
    }
}

/// Errors of [`decode`], [`encode_indexed`] and [`compress_png`].
#[derive(Debug)]
#[non_exhaustive]
pub enum PngError {
    /// The input is not a valid PNG, or uses a feature the png crate doesn't support.
    Decode(Box<dyn std::error::Error + Send + Sync>),
    /// The png crate could not write the image.
    Encode(Box<dyn std::error::Error + Send + Sync>),
    /// Quantization failed (for example [`Error::QualityTooLow`]), or an argument was out of
    /// range.
    Quantize(Error),
}

impl fmt::Display for PngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "invalid PNG: {e}"),
            Self::Encode(e) => write!(f, "PNG encoding failed: {e}"),
            Self::Quantize(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for PngError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(e) | Self::Encode(e) => Some(&**e),
            Self::Quantize(e) => Some(e),
        }
    }
}

impl From<Error> for PngError {
    fn from(e: Error) -> Self {
        Self::Quantize(e)
    }
}

/// Decodes any PNG to 8-bit RGBA: palettes and tRNS are expanded, 16-bit samples are
/// reduced to 8 bits, gray is copied to all three channels. Interlaced images are
/// deinterlaced; of an animated PNG, the default image is read.
///
/// The png crate's memory limit is lifted, since the caller chose the input; the pixel
/// buffer is allocated fallibly.
///
/// # Errors
///
/// [`PngError::Decode`] if the data is not a valid PNG, [`PngError::Quantize`] with
/// [`Error::OutOfMemory`] if the pixels don't fit in memory.
pub fn decode(data: &[u8]) -> Result<DecodedPng, PngError> {
    let decode_error = |e: png::DecodingError| PngError::Decode(Box::new(e));
    let limits = png::Limits { bytes: usize::MAX };
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(data), limits);
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(decode_error)?;
    let gamma = {
        let info = reader.info();
        let gama = info.gama_chunk.filter(|_| info.srgb.is_none());
        gama.map(|g| f64::from(g.into_scaled()) / 100_000.0)
    };
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| decode_error(png::DecodingError::LimitsExceeded))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(size).map_err(|_| Error::OutOfMemory)?;
    buf.resize(size, 0);
    let frame = reader.next_frame(&mut buf).map_err(decode_error)?;
    let bytes = &buf[..frame.buffer_size()];
    let pixels = match frame.color_type {
        png::ColorType::Rgba => bytes
            .chunks_exact(4)
            .map(|p| RGBA::new(p[0], p[1], p[2], p[3]))
            .collect(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .map(|p| RGBA::new(p[0], p[1], p[2], 255))
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .map(|p| RGBA::new(p[0], p[0], p[0], p[1]))
            .collect(),
        png::ColorType::Grayscale => bytes.iter().map(|&g| RGBA::new(g, g, g, 255)).collect(),
        png::ColorType::Indexed => unreachable!("palette images are expanded"),
    };
    Ok(DecodedPng {
        pixels,
        width: frame.width as usize,
        height: frame.height as usize,
        gamma,
    })
}

/// Encodes a palette image as small as the png crate can: twice, unfiltered and with
/// minimum-entropy filters per row, keeping the smaller file (in parallel with the `threads`
/// feature).
///
/// - Filter type None is what PNG spec section 12.7 recommends for palette images, whose
///   indices are usually not numerically smooth; per-row filters win when the palette is
///   ordered so that touching colors get close indices, as [`crate::Profile::Improved`]
///   does. The png crate's default (adaptive filtering alone) is worse than both.
/// - The bit depth is the smallest that holds the palette (1, 2, 4 or 8 bits per pixel).
/// - The tRNS chunk stops after the last non-opaque entry (translucent entries come first in
///   this crate's palettes).
/// - zlib level `Compression::High`.
///
/// `indices` holds `width * height` palette indices, row by row. `gamma`, if given, is
/// written as the gAMA chunk.
///
/// # Errors
///
/// - [`PngError::Quantize`] with [`Error::ValueOutOfRange`] if the size is 0 or over
///   `u32::MAX`, the palette is empty or has more than 256 colors, an index is outside the
///   palette, or `gamma` is negative or not finite; with [`Error::BufferTooSmall`] if there
///   are fewer than `width * height` indices.
/// - [`PngError::Encode`] if the png crate fails.
pub fn encode_indexed(
    palette: &[RGBA],
    indices: &[u8],
    width: usize,
    height: usize,
    gamma: Option<f64>,
) -> Result<Vec<u8>, PngError> {
    let in_u32 = |v: usize| v > 0 && u32::try_from(v).is_ok();
    if !in_u32(width) || !in_u32(height) || !(1..=256).contains(&palette.len()) {
        return Err(Error::ValueOutOfRange.into());
    }
    let pixels = width.checked_mul(height).ok_or(Error::ValueOutOfRange)?;
    let indices = indices.get(..pixels).ok_or(Error::BufferTooSmall)?;
    // a larger index would spill into the neighboring pixels when packed
    if indices.iter().any(|&i| usize::from(i) >= palette.len()) {
        return Err(Error::ValueOutOfRange.into());
    }
    let gama = match gamma {
        Some(g) if g.is_finite() && g >= 0.0 && g * 100_000.0 <= f64::from(u32::MAX) => {
            Some(png::ScaledFloat::from_scaled((g * 100_000.0).round() as u32))
        }
        Some(_) => return Err(Error::ValueOutOfRange.into()),
        None => None,
    };

    let encode = |filter| encode_with(width, height, palette, indices, gama, filter);
    // the two encodings are independent; with threads, they run at the same time
    #[cfg(feature = "threads")]
    let (unfiltered, filtered) = rayon::join(|| encode(png::Filter::NoFilter), || encode(png::Filter::MinEntropy));
    #[cfg(not(feature = "threads"))]
    let (unfiltered, filtered) = (encode(png::Filter::NoFilter), encode(png::Filter::MinEntropy));
    let encode_error = |e: png::EncodingError| PngError::Encode(Box::new(e));
    let (unfiltered, filtered) = (unfiltered.map_err(encode_error)?, filtered.map_err(encode_error)?);
    Ok(if filtered.len() < unfiltered.len() {
        filtered
    } else {
        unfiltered
    })
}

fn encode_with(
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
    gama: Option<png::ScaledFloat>,
    filter: png::Filter,
) -> Result<Vec<u8>, png::EncodingError> {
    let (bits, depth) = match palette.len() {
        0..=2 => (1, png::BitDepth::One),
        3..=4 => (2, png::BitDepth::Two),
        5..=16 => (4, png::BitDepth::Four),
        _ => (8, png::BitDepth::Eight),
    };
    let mut out = Vec::new();
    // checked by the caller
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(depth);
    // set_compression also resets the filter (to Adaptive for High), so it must come first
    encoder.set_compression(png::Compression::High);
    encoder.set_filter(filter);
    if let Some(gama) = gama {
        encoder.set_source_gamma(gama);
    }
    encoder.set_palette(palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect::<Vec<_>>());
    let trns_len = palette.iter().rposition(|c| c.a != 255).map_or(0, |i| i + 1);
    if trns_len > 0 {
        encoder.set_trns(palette[..trns_len].iter().map(|c| c.a).collect::<Vec<_>>());
    }
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&pack_indices(indices, width, bits))?;
    writer.finish()?;
    Ok(out)
}

/// Packs 8-bit indices into `bits`-bit samples, leftmost pixel in the most significant
/// bits, each row padded to whole bytes (PNG spec section 7.2).
fn pack_indices(indices: &[u8], width: usize, bits: usize) -> Vec<u8> {
    if bits == 8 || width == 0 {
        return indices.to_vec();
    }
    let per_byte = 8 / bits;
    let stride = width.div_ceil(per_byte);
    let mut packed = vec![0u8; stride * (indices.len() / width)];
    for (row, out) in indices.chunks_exact(width).zip(packed.chunks_exact_mut(stride)) {
        for (x, &i) in row.iter().enumerate() {
            out[x / per_byte] |= i << (8 - bits * (x % per_byte + 1));
        }
    }
    packed
}

/// Converts a PNG to a palette PNG, like pngquant: decodes it ([`decode`]), quantizes it
/// with `attr`, remaps it with dithering level `dither` (0-1), and encodes the result
/// ([`encode_indexed`]) with the input's gamma.
///
/// Returns the PNG file and the quality reached (0-100, as in
/// [`Attributes::set_quality`]; `None` if it was not measured).
///
/// # Errors
///
/// As [`decode`] and [`encode_indexed`]; [`PngError::Quantize`] with the errors of
/// [`Attributes::quantize`], in particular [`Error::QualityTooLow`] when the minimum
/// quality can't be reached.
pub fn compress_png(input: &[u8], attr: &Attributes, dither: f32) -> Result<(Vec<u8>, Option<u8>), PngError> {
    let decoded = decode(input)?;
    let mut image = attr.new_image_borrowed(&decoded.pixels, decoded.width, decoded.height, decoded.image_gamma())?;
    let mut result = attr.quantize(&mut image)?;
    result.set_dithering_level(dither)?;
    let (palette, indices) = result.remapped(&mut image)?;
    let output = encode_indexed(&palette, &indices, decoded.width, decoded.height, decoded.gamma)?;
    Ok((output, result.quantization_quality()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_known_bytes() {
        assert_eq!(
            pack_indices(&[1, 0, 1, 1, 0, 0, 0, 1, 1], 9, 1),
            [0b1011_0001, 0b1000_0000]
        );
        assert_eq!(pack_indices(&[3, 0, 2, 1, 1, 1], 3, 2), [0b1100_1000, 0b0101_0100]);
        assert_eq!(pack_indices(&[0xa, 0x5, 0xf], 3, 4), [0xa5, 0xf0]);
    }

    fn rgba_png(setup: impl FnOnce(&mut png::Encoder<'_, &mut Vec<u8>>)) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 3, 2);
        encoder.set_color(png::ColorType::Rgba);
        setup(&mut encoder);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..6u8).flat_map(|i| [i * 40, 255 - i * 40, 7, 255 - i]).collect();
        writer.write_image_data(&data).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn gamma_handling() {
        let decoded = decode(&rgba_png(|e| e.set_source_gamma(png::ScaledFloat::from_scaled(50_000)))).unwrap();
        assert_eq!((decoded.gamma, decoded.image_gamma()), (Some(0.5), 0.5));
        // out of the quantizer's range: quantize as sRGB, but keep the chunk
        let decoded = decode(&rgba_png(|e| {
            e.set_source_gamma(png::ScaledFloat::from_scaled(220_000))
        }))
        .unwrap();
        assert_eq!((decoded.gamma, decoded.image_gamma()), (Some(2.2), 0.0));
        // sRGB overrides gAMA
        let decoded = decode(&rgba_png(|e| {
            e.set_source_gamma(png::ScaledFloat::from_scaled(50_000));
            e.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        }))
        .unwrap();
        assert_eq!((decoded.gamma, decoded.image_gamma()), (None, 0.0));
        let decoded = decode(&rgba_png(|_| {})).unwrap();
        assert_eq!((decoded.gamma, decoded.width, decoded.height), (None, 3, 2));
        assert_eq!(decoded.pixels[1], RGBA::new(40, 215, 7, 254));
        // scaled gamma values survive the round trip exactly
        for scaled in [1, 45_455, 55_000, 99_999, 220_000] {
            let g = f64::from(scaled) / 100_000.0;
            let out = encode_indexed(&[RGBA::new(1, 2, 3, 255)], &[0; 6], 3, 2, Some(g)).unwrap();
            let info = png::Decoder::new(Cursor::new(&out)).read_info().unwrap();
            assert_eq!(info.info().gama_chunk.map(png::ScaledFloat::into_scaled), Some(scaled));
        }
    }

    #[test]
    fn encode_checks_arguments() {
        let palette = [RGBA::new(0, 0, 0, 255), RGBA::new(255, 255, 255, 128)];
        let ok = encode_indexed(&palette, &[0, 1, 1, 0], 2, 2, None).unwrap();
        let decoded = decode(&ok).unwrap();
        assert_eq!(decoded.pixels[1], palette[1]);
        let invalid = |r: Result<Vec<u8>, PngError>| match r {
            Err(PngError::Quantize(e)) => e,
            _ => panic!("expected an argument error"),
        };
        assert_eq!(
            invalid(encode_indexed(&palette, &[0, 1, 2, 0], 2, 2, None)),
            Error::ValueOutOfRange
        );
        assert_eq!(
            invalid(encode_indexed(&palette, &[0, 1, 1], 2, 2, None)),
            Error::BufferTooSmall
        );
        assert_eq!(
            invalid(encode_indexed(&[], &[0; 4], 2, 2, None)),
            Error::ValueOutOfRange
        );
        assert_eq!(
            invalid(encode_indexed(&palette, &[], 0, 2, None)),
            Error::ValueOutOfRange
        );
        assert_eq!(
            invalid(encode_indexed(&palette, &[0; 4], 2, 2, Some(-1.0))),
            Error::ValueOutOfRange
        );
        assert_eq!(
            invalid(encode_indexed(&palette, &[0; 4], 2, 2, Some(f64::NAN))),
            Error::ValueOutOfRange
        );
        assert!(matches!(decode(b"not a png"), Err(PngError::Decode(_))));
    }
}
