//! PNG reading (any color type and bit depth) and size-conscious indexed PNG writing.

use std::io::Cursor;

use libimgquant::RGBA;

/// A decoded PNG as 8-bit RGBA, with its color space chunks.
pub struct Decoded {
    pub pixels: Vec<RGBA>,
    pub width: usize,
    pub height: usize,
    /// The gAMA chunk, if any.
    pub gama: Option<png::ScaledFloat>,
    /// Whether the image has an sRGB chunk, which overrides gAMA.
    pub srgb: bool,
}

impl Decoded {
    /// Gamma for the quantizer: the gAMA value when it is usable, 0 (sRGB) otherwise.
    pub fn quantizer_gamma(&self) -> f64 {
        match self.gama {
            Some(g) if !self.srgb => {
                let gamma = f64::from(g.into_scaled()) / 100_000.0;
                if gamma > 0.0 && gamma <= 1.0 { gamma } else { 0.0 }
            }
            _ => 0.0,
        }
    }

    /// The gAMA chunk to write to the output: the input's, unless an sRGB chunk overrides it.
    pub fn output_gama(&self) -> Option<png::ScaledFloat> {
        self.gama.filter(|_| !self.srgb)
    }
}

/// Decodes any PNG to 8-bit RGBA: palettes and tRNS are expanded, 16-bit samples are
/// reduced to 8 bits, gray is copied to all three channels. Interlaced images are
/// deinterlaced; of an animated PNG, the default image is read.
pub fn decode(data: &[u8]) -> Result<Decoded, png::DecodingError> {
    // the size of the input is the user's choice; libimgquant has its own limit
    let limits = png::Limits { bytes: usize::MAX };
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(data), limits);
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let (gama, srgb) = {
        let info = reader.info();
        (info.gama_chunk, info.srgb.is_some())
    };
    let size = reader.output_buffer_size().ok_or(png::DecodingError::LimitsExceeded)?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf)?;
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
    Ok(Decoded {
        pixels,
        width: frame.width as usize,
        height: frame.height as usize,
        gama,
        srgb,
    })
}

/// Encodes an indexed PNG twice, unfiltered and with minimum-entropy filters per row, and
/// returns the smaller file.
///
/// - Filter type None is what PNG spec section 12.7 recommends for palette images, whose
///   indices are usually not numerically smooth; per-row filters win when the palette is
///   ordered so that touching colors get close indices, as `Profile::Improved` does. The
///   png crate's default (adaptive filtering alone) is worse than both.
/// - The bit depth is the smallest that holds the palette (1, 2, 4 or 8 bits per pixel).
/// - The tRNS chunk stops after the last non-opaque entry (translucent entries come first).
/// - zlib level `Compression::High`.
pub fn encode_indexed(
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
    gama: Option<png::ScaledFloat>,
) -> Result<Vec<u8>, png::EncodingError> {
    let encode = |filter| encode_with(width, height, palette, indices, gama, filter);
    // the two encodings are independent; with threads, they run at the same time
    #[cfg(feature = "threads")]
    let (unfiltered, filtered) = rayon::join(|| encode(png::Filter::NoFilter), || encode(png::Filter::MinEntropy));
    #[cfg(not(feature = "threads"))]
    let (unfiltered, filtered) = (encode(png::Filter::NoFilter), encode(png::Filter::MinEntropy));
    let (unfiltered, filtered) = (unfiltered?, filtered?);
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
    // the dimensions came from a PNG, so they fit in u32
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

    #[test]
    fn gamma_handling() {
        let mut d = Decoded {
            pixels: Vec::new(),
            width: 0,
            height: 0,
            gama: Some(png::ScaledFloat::from_scaled(50_000)),
            srgb: false,
        };
        assert_eq!(d.quantizer_gamma(), 0.5);
        assert_eq!(d.output_gama(), d.gama);
        // out of the quantizer's range: quantize as sRGB, but keep the chunk
        d.gama = Some(png::ScaledFloat::from_scaled(220_000));
        assert_eq!(d.quantizer_gamma(), 0.0);
        assert_eq!(d.output_gama(), d.gama);
        d.srgb = true;
        assert_eq!(d.quantizer_gamma(), 0.0);
        assert_eq!(d.output_gama(), None);
    }
}
