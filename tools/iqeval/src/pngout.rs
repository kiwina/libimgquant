//! Indexed PNG writing, the way a size-conscious encoder should do it.
//!
//! - Two encodings, keeping the smaller: filter type None, which PNG spec section 12.7 (and
//!   libpng) recommend for palette images because indices are usually not numerically
//!   smooth, and per-row filters chosen by minimum entropy, which win when the palette is
//!   ordered so that neighbouring colors have close indices (as the `Improved` profile
//!   does). The png crate's default, adaptive filtering on its own, is worse than both.
//! - The smallest bit depth that holds the palette (1, 2, 4 or 8 bits per pixel).
//! - A tRNS chunk trimmed after the last non-opaque entry (the quantizer sorts transparent
//!   entries first, so this is usually short or absent).
//! - zlib level `Compression::High`.

use libimgquant::RGBA;

/// Bits per pixel needed for indices into a palette of `colors` entries.
#[must_use]
pub fn bit_depth(colors: usize) -> u8 {
    match colors {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// Packs 8-bit indices into `bits`-bit samples, leftmost pixel in the most significant
/// bits, each row padded to whole bytes (PNG spec section 7.2).
#[must_use]
pub fn pack_indices(indices: &[u8], width: usize, bits: u8) -> Vec<u8> {
    if bits == 8 || width == 0 {
        return indices.to_vec();
    }
    let per_byte = usize::from(8 / bits);
    let stride = width.div_ceil(per_byte);
    let rows = indices.len() / width;
    let mut packed = vec![0u8; stride * rows];
    for (row, out) in indices.chunks_exact(width).zip(packed.chunks_exact_mut(stride)) {
        for (x, &i) in row.iter().enumerate() {
            debug_assert!(u16::from(i) < 1 << bits, "index {i} does not fit in {bits} bits");
            let shift = 8 - bits as usize * (x % per_byte + 1);
            out[x / per_byte] |= i << shift;
        }
    }
    packed
}

/// Encodes an indexed PNG in memory, unfiltered and with minimum-entropy filters, and
/// returns the smaller file.
///
/// # Errors
/// Encoder errors (only for invalid dimensions or a palette over 256 entries).
pub fn encode_indexed(
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
) -> Result<Vec<u8>, png::EncodingError> {
    let unfiltered = encode_indexed_with(width, height, palette, indices, png::Filter::NoFilter)?;
    let filtered = encode_indexed_with(width, height, palette, indices, png::Filter::MinEntropy)?;
    Ok(if filtered.len() < unfiltered.len() {
        filtered
    } else {
        unfiltered
    })
}

/// Encodes an indexed PNG in memory with one filter strategy.
///
/// # Errors
/// Encoder errors (only for invalid dimensions or a palette over 256 entries).
pub fn encode_indexed_with(
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
    filter: png::Filter,
) -> Result<Vec<u8>, png::EncodingError> {
    let bits = bit_depth(palette.len());
    let depth = match bits {
        1 => png::BitDepth::One,
        2 => png::BitDepth::Two,
        4 => png::BitDepth::Four,
        _ => png::BitDepth::Eight,
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(depth);
    // set_compression also resets the filter (to Adaptive for High), so it must come first
    encoder.set_compression(png::Compression::High);
    encoder.set_filter(filter);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(data: &[u8]) -> (png::OutputInfo, Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        decoder.set_transformations(png::Transformations::IDENTITY);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        let header = reader.info();
        let plte = header.palette.as_ref().unwrap().to_vec();
        let trns = header.trns.as_ref().map(|t| t.to_vec());
        (info, buf, plte, trns)
    }

    fn unpack(packed: &[u8], width: usize, height: usize, bits: u8) -> Vec<u8> {
        let stride = (width * bits as usize).div_ceil(8);
        let mask = ((1u16 << bits) - 1) as u8;
        let mut out = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let bit = x * bits as usize;
                let byte = packed[y * stride + bit / 8];
                out.push((byte >> (8 - bits as usize - bit % 8)) & mask);
            }
        }
        out
    }

    #[test]
    fn depth_thresholds() {
        assert_eq!(bit_depth(1), 1);
        assert_eq!(bit_depth(2), 1);
        assert_eq!(bit_depth(3), 2);
        assert_eq!(bit_depth(4), 2);
        assert_eq!(bit_depth(5), 4);
        assert_eq!(bit_depth(16), 4);
        assert_eq!(bit_depth(17), 8);
        assert_eq!(bit_depth(256), 8);
    }

    #[test]
    fn pack_known_bytes() {
        // 1 bit: 1,0,1,1,0,0,0,1 | 1 -> 0b1011_0001, 0b1000_0000
        assert_eq!(
            pack_indices(&[1, 0, 1, 1, 0, 0, 0, 1, 1], 9, 1),
            [0b1011_0001, 0b1000_0000]
        );
        // 2 bits, two rows of 3 pixels, each padded to one byte
        assert_eq!(pack_indices(&[3, 0, 2, 1, 1, 1], 3, 2), [0b1100_1000, 0b0101_0100]);
        // 4 bits
        assert_eq!(pack_indices(&[0xa, 0x5, 0xf], 3, 4), [0xa5, 0xf0]);
    }

    #[test]
    fn roundtrip_every_depth_and_odd_widths() {
        for colors in [1usize, 2, 3, 4, 7, 16, 17, 256] {
            for width in [1usize, 3, 7, 8, 9, 13] {
                let height = 5;
                let palette: Vec<RGBA> = (0..colors)
                    .map(|i| RGBA::new(i as u8, (i * 7) as u8, 255 - i as u8, if i < 2 { 128 } else { 255 }))
                    .collect();
                let indices: Vec<u8> = (0..width * height).map(|i| ((i * 31 + i / 3) % colors) as u8).collect();
                let data = encode_indexed(width, height, &palette, &indices).unwrap();
                let (info, buf, plte, trns) = decode(&data);
                let bits = bit_depth(colors);
                assert_eq!(info.bit_depth as u8, bits);
                assert_eq!(info.color_type, png::ColorType::Indexed);
                assert_eq!(
                    unpack(&buf, width, height, bits),
                    indices,
                    "colors {colors} width {width}"
                );
                assert_eq!(plte.len(), colors * 3);
                // only the leading transparent entries are stored
                assert_eq!(trns.as_deref(), Some(&[128u8, 128][..colors.min(2)]));
            }
        }
    }

    /// Concatenated, inflated IDAT data: one filter-type byte, then the row's bytes.
    fn raw_scanlines(data: &[u8]) -> Vec<u8> {
        let mut idat = Vec::new();
        let mut pos = 8;
        while pos + 8 <= data.len() {
            let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            if &data[pos + 4..pos + 8] == b"IDAT" {
                idat.extend_from_slice(&data[pos + 8..pos + 8 + len]);
            }
            pos += 12 + len;
        }
        miniz_oxide::inflate::decompress_to_vec_zlib(&idat).unwrap()
    }

    #[test]
    fn keeps_the_smaller_encoding() {
        let (width, height) = (64, 32);
        let palette: Vec<RGBA> = (0..=255).map(|i| RGBA::new(i, 0, 0, 255)).collect();
        // a smooth ramp of indices, where filters help, and noise, where they don't
        let ramp: Vec<u8> = (0..width * height).map(|i| ((i % width) * 4) as u8).collect();
        let noise: Vec<u8> = (0..width * height)
            .map(|i| ((i * 2_654_435_761usize) >> 13) as u8)
            .collect();
        for indices in [ramp, noise] {
            let best = encode_indexed(width, height, &palette, &indices).unwrap().len();
            let none = encode_indexed_with(width, height, &palette, &indices, png::Filter::NoFilter).unwrap();
            let min_entropy = encode_indexed_with(width, height, &palette, &indices, png::Filter::MinEntropy).unwrap();
            assert_eq!(best, none.len().min(min_entropy.len()));
        }
    }

    #[test]
    fn every_row_uses_filter_none() {
        // a smooth gradient, which the adaptive filter would encode with Sub/Up/Paeth
        for colors in [4usize, 16, 256] {
            let (width, height) = (61, 9);
            let palette: Vec<RGBA> = (0..colors).map(|i| RGBA::new(i as u8, 0, 0, 255)).collect();
            let indices: Vec<u8> = (0..width * height)
                .map(|i| ((i % width + i / width) % colors) as u8)
                .collect();
            let data = encode_indexed_with(width, height, &palette, &indices, png::Filter::NoFilter).unwrap();
            let raw = raw_scanlines(&data);
            let stride = (width * bit_depth(colors) as usize).div_ceil(8);
            assert_eq!(raw.len(), height * (stride + 1));
            for row in raw.chunks_exact(stride + 1) {
                assert_eq!(row[0], 0, "filter type for {colors} colors");
            }
        }
    }

    #[test]
    fn opaque_palette_has_no_trns() {
        let palette = [RGBA::new(0, 0, 0, 255), RGBA::new(255, 255, 255, 255)];
        let data = encode_indexed(2, 1, &palette, &[0, 1]).unwrap();
        assert!(decode(&data).3.is_none());
    }
}
