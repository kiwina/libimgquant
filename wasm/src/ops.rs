//! The three operations on plain Rust values, with errors as the messages the JS `Error`s
//! carry. The bindings in `lib.rs` only convert JS values.

use libimgquant::png_io::{self, PngError};
use libimgquant::{Attributes, Error, Histogram, Image, RGBA};
use rgb::FromSlice;

use crate::options::Settings;

/// A quantized image: RGBA bytes of the palette, one index per pixel, and the quality reached.
#[derive(Debug)]
pub struct Quantized {
    pub palette: Vec<u8>,
    pub indices: Vec<u8>,
    pub quality: Option<u8>,
}

/// One palette for several frames, and each frame's indices.
#[derive(Debug)]
pub struct QuantizedFrames {
    pub palette: Vec<u8>,
    pub frames: Vec<Vec<u8>>,
}

/// Width and height as JS numbers: positive integers whose RGBA size fits in memory.
/// Returns them with the expected byte length.
pub fn dimensions(width: f64, height: f64) -> Result<(usize, usize, usize), String> {
    let side = |v: f64| (v.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&v)).then_some(v as usize);
    let (Some(w), Some(h)) = (side(width), side(height)) else {
        return Err(format!(
            "width and height must be positive integers, not {width} and {height}"
        ));
    };
    let bytes = w
        .checked_mul(h)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| format!("{w}x{h} is too large"))?;
    Ok((w, h, bytes))
}

fn check_length(what: &str, len: usize, width: usize, height: usize, bytes: usize) -> Result<(), String> {
    if len == bytes {
        Ok(())
    } else {
        Err(format!(
            "{what} has {len} bytes, but {width}x{height} RGBA pixels need {bytes} (4 bytes per pixel)"
        ))
    }
}

/// The message for a library error.
fn message(e: Error, settings: &Settings) -> String {
    match e {
        Error::QualityTooLow => format!(
            "quality too low: the image can't reach the minimum quality {} with {} colors",
            settings.quality.map_or(0, |q| q.0),
            settings.max_colors
        ),
        Error::OutOfMemory => "out of memory: the image is too large for WebAssembly's 4 GB".to_string(),
        Error::ValueOutOfRange => "the image is too large (at most 2^31 - 1 pixels)".to_string(),
        e => e.to_string(),
    }
}

fn new_image<'a>(attr: &Attributes, rgba: &'a [u8], width: usize, height: usize) -> Result<Image<'a>, Error> {
    attr.new_image_borrowed(rgba.as_rgba(), width, height, 0.0)
}

fn palette_bytes(palette: &[RGBA]) -> Vec<u8> {
    palette.iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect()
}

/// Quantizes and remaps one image of `width * height` RGBA pixels.
pub fn quantize(rgba: &[u8], width: f64, height: f64, settings: &Settings) -> Result<Quantized, String> {
    let (width, height, bytes) = dimensions(width, height)?;
    check_length("the pixel data", rgba.len(), width, height, bytes)?;
    let attr = settings.attributes()?;
    let run = || {
        let mut image = new_image(&attr, rgba, width, height)?;
        let mut result = attr.quantize(&mut image)?;
        result.set_dithering_level(settings.dithering)?;
        let (palette, indices) = result.remapped(&mut image)?;
        Ok(Quantized {
            palette: palette_bytes(&palette),
            indices,
            quality: result.quantization_quality(),
        })
    };
    run().map_err(|e| message(e, settings))
}

/// Converts a PNG file to a palette PNG, keeping its gamma and RGB ICC profile.
pub fn compress_png(png: &[u8], settings: &Settings) -> Result<Vec<u8>, String> {
    let attr = settings.attributes()?;
    match png_io::compress_png(png, &attr, settings.dithering) {
        Ok((output, _)) => Ok(output),
        Err(PngError::Quantize(e)) => Err(message(e, settings)),
        Err(e) => Err(e.to_string()),
    }
}

/// Quantizes all frames to one palette with a [`Histogram`] and remaps each of them.
///
/// Each frame's image is dropped after its colors are counted and created again for
/// remapping, so only one frame's converted pixels are in memory at a time.
pub fn quantize_frames<F: AsRef<[u8]>>(
    frames: &[F],
    width: f64,
    height: f64,
    settings: &Settings,
) -> Result<QuantizedFrames, String> {
    let (width, height, bytes) = dimensions(width, height)?;
    if frames.is_empty() {
        return Err("frames must contain at least one frame".to_string());
    }
    for (i, frame) in frames.iter().enumerate() {
        check_length(&format!("frame {i}"), frame.as_ref().len(), width, height, bytes)?;
    }
    let attr = settings.attributes()?;
    let run = || {
        let mut histogram = Histogram::new(&attr);
        for frame in frames {
            histogram.add_image(&attr, &mut new_image(&attr, frame.as_ref(), width, height)?)?;
        }
        let mut result = histogram.quantize(&attr)?;
        result.set_dithering_level(settings.dithering)?;
        let palette = palette_bytes(&result.palette_vec());
        let frames = frames
            .iter()
            .map(|frame| {
                let mut image = new_image(&attr, frame.as_ref(), width, height)?;
                result.remapped(&mut image).map(|(_, indices)| indices)
            })
            .collect::<Result<_, Error>>()?;
        Ok(QuantizedFrames { palette, frames })
    };
    run().map_err(|e| message(e, settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gradient with a translucent band at the top.
    fn gradient(width: usize, height: usize, shift: u8) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                let a = if y < 4 { 120 } else { 255 };
                [(x * 4) as u8, (y * 5) as u8, shift, a]
            })
            .collect()
    }

    #[test]
    fn dimension_rules() {
        assert_eq!(dimensions(3.0, 2.0), Ok((3, 2, 24)));
        for (w, h) in [(0.0, 1.0), (1.0, -2.0), (1.5, 2.0), (f64::NAN, 1.0), (1.0, 5e9)] {
            assert!(
                dimensions(w, h)
                    .unwrap_err()
                    .starts_with("width and height must be positive integers")
            );
        }
    }

    #[test]
    fn quantizes_one_image() {
        let pixels = gradient(64, 48, 90);
        let settings = Settings {
            max_colors: 32,
            ..Settings::default()
        };
        let q = quantize(&pixels, 64.0, 48.0, &settings).unwrap();
        assert!(q.palette.len() <= 32 * 4 && q.palette.len() % 4 == 0);
        assert_eq!(q.indices.len(), 64 * 48);
        assert!(q.indices.iter().all(|&i| usize::from(i) < q.palette.len() / 4));
        // translucent entries first
        assert!(q.palette[3] < 255);
        assert!(q.quality.is_some());
    }

    #[test]
    fn length_and_quality_errors() {
        let pixels = gradient(8, 8, 0);
        assert_eq!(
            quantize(&pixels, 8.0, 9.0, &Settings::default()).unwrap_err(),
            "the pixel data has 256 bytes, but 8x9 RGBA pixels need 288 (4 bytes per pixel)"
        );
        let strict = Settings {
            max_colors: 2,
            quality: Some((90, 100)),
            ..Settings::default()
        };
        assert_eq!(
            quantize(&gradient(64, 64, 0), 64.0, 64.0, &strict).unwrap_err(),
            "quality too low: the image can't reach the minimum quality 90 with 2 colors"
        );
    }

    #[test]
    fn frames_share_a_palette() {
        let frames = [gradient(32, 32, 0), gradient(32, 32, 200)];
        let q = quantize_frames(&frames, 32.0, 32.0, &Settings::default()).unwrap();
        assert_eq!(q.frames.len(), 2);
        let colors = q.palette.len() / 4;
        for indices in &q.frames {
            assert_eq!(indices.len(), 32 * 32);
            assert!(indices.iter().all(|&i| usize::from(i) < colors));
        }
        // blue 0 and blue 200 both have palette entries
        let blues: Vec<u8> = q.palette.chunks(4).map(|c| c[2]).collect();
        assert!(blues.iter().any(|&b| b < 50) && blues.iter().any(|&b| b > 150));

        let none: [Vec<u8>; 0] = [];
        assert!(quantize_frames(&none, 1.0, 1.0, &Settings::default()).is_err());
        assert_eq!(
            quantize_frames(&[gradient(2, 2, 0), vec![0; 4]], 2.0, 2.0, &Settings::default()).unwrap_err(),
            "frame 1 has 4 bytes, but 2x2 RGBA pixels need 16 (4 bytes per pixel)"
        );
    }

    #[test]
    fn compresses_png() {
        let pixels = gradient(64, 48, 90);
        let mut input = Vec::new();
        let mut encoder = png::Encoder::new(&mut input, 64, 48);
        encoder.set_color(png::ColorType::Rgba);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixels).unwrap();
        writer.finish().unwrap();

        let output = compress_png(&input, &Settings::default()).unwrap();
        assert_eq!(output[25], 3, "IHDR color type: indexed");
        let decoded = png_io::decode(&output).unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 48));
        assert!(
            compress_png(b"not a png", &Settings::default())
                .unwrap_err()
                .starts_with("invalid PNG")
        );
    }
}
