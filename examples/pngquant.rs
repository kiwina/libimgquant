//! Converts a PNG to an 8-bit palette PNG, like a minimal pngquant.
//!
//! ```text
//! cargo run --release --example pngquant -- input.png output.png [--speed N] [--quality MIN-MAX] [--colors N]
//!     [--nofs] [--profile compat|improved]
//! ```
//!
//! The output is written with filter None, the smallest bit depth for the palette and a
//! trimmed tRNS chunk; see `write_png` for why.

use std::fs::File;
use std::io::BufReader;
use std::time::Instant;

use libimgquant::RGBA;

struct Options {
    input: String,
    output: String,
    speed: i32,
    quality: Option<(u8, u8)>,
    colors: u32,
    dither: f32,
    profile: libimgquant::Profile,
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut opts = Options {
        input: String::new(),
        output: String::new(),
        speed: 3,
        quality: None,
        colors: 256,
        dither: 1.0,
        profile: libimgquant::Profile::Compat,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--speed" => opts.speed = value("--speed")?.parse().map_err(|e| format!("--speed: {e}"))?,
            "--colors" => opts.colors = value("--colors")?.parse().map_err(|e| format!("--colors: {e}"))?,
            "--quality" => {
                let v = value("--quality")?;
                let (min, max) = v.split_once('-').ok_or("--quality takes MIN-MAX")?;
                opts.quality = Some((
                    min.parse().map_err(|e| format!("--quality: {e}"))?,
                    max.parse().map_err(|e| format!("--quality: {e}"))?,
                ));
            }
            "--nofs" => opts.dither = 0.0,
            "--profile" => {
                opts.profile = match value("--profile")?.as_str() {
                    "compat" => libimgquant::Profile::Compat,
                    "improved" => libimgquant::Profile::Improved,
                    other => return Err(format!("unknown profile {other}")),
                }
            }
            _ => positional.push(arg),
        }
    }
    let [input, output] =
        <[String; 2]>::try_from(positional).map_err(|_| "expected input.png output.png".to_string())?;
    opts.input = input;
    opts.output = output;
    Ok(opts)
}

fn read_png(path: &str) -> Result<(Vec<RGBA>, usize, usize), Box<dyn std::error::Error>> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut buf)?;
    let bytes = &buf[..info.buffer_size()];
    let pixels = match info.color_type {
        png::ColorType::Rgba => bytes
            .chunks_exact(4)
            .map(|p| RGBA::new(p[0], p[1], p[2], p[3]))
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .map(|p| RGBA::new(p[0], p[0], p[0], p[1]))
            .collect(),
        other => return Err(format!("unexpected color type {other:?}").into()),
    };
    Ok((pixels, info.width as usize, info.height as usize))
}

/// Writes an indexed PNG the way a size-conscious encoder should:
///
/// - the smaller of two encodings: filter type None, which PNG spec section 12.7 recommends
///   for palette images (indices are usually not numerically smooth; the png crate's
///   default of adaptive filtering makes these files about 15-20% larger), and per-row
///   filters chosen by minimum entropy, which win when the palette is ordered so that
///   touching colors have close indices, as `Profile::Improved` does,
/// - the smallest bit depth that holds the palette (1, 2, 4 or 8 bits per pixel),
/// - a tRNS chunk that stops at the last non-opaque entry (the quantizer puts transparent
///   entries first, so it is short or absent),
/// - zlib `Compression::High`.
fn write_png(
    path: &str,
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let unfiltered = encode_png(width, height, palette, indices, png::Filter::NoFilter)?;
    let filtered = encode_png(width, height, palette, indices, png::Filter::MinEntropy)?;
    let best = if filtered.len() < unfiltered.len() {
        filtered
    } else {
        unfiltered
    };
    std::fs::write(path, best)?;
    Ok(())
}

fn encode_png(
    width: usize,
    height: usize,
    palette: &[RGBA],
    indices: &[u8],
    filter: png::Filter,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (bits, depth) = match palette.len() {
        0..=2 => (1, png::BitDepth::One),
        3..=4 => (2, png::BitDepth::Two),
        5..=16 => (4, png::BitDepth::Four),
        _ => (8, png::BitDepth::Eight),
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

/// Packs 8-bit indices into `bits`-bit samples, leftmost pixel in the most significant
/// bits, each row padded to a whole byte (PNG spec section 7.2).
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_args().inspect_err(|_| {
        eprintln!(
            "usage: pngquant input.png output.png [--speed N] [--quality MIN-MAX] [--colors N] [--nofs] [--profile compat|improved]"
        );
    })?;
    let (pixels, width, height) = read_png(&opts.input)?;

    let mut attr = libimgquant::new();
    attr.set_profile(opts.profile);
    attr.set_speed(opts.speed)?;
    attr.set_max_colors(opts.colors)?;
    if let Some((min, max)) = opts.quality {
        attr.set_quality(min, max)?;
    }

    let start = Instant::now();
    let mut image = attr.new_image(pixels, width, height, 0.0)?;
    let mut result = attr.quantize(&mut image)?;
    result.set_dithering_level(opts.dither)?;
    let (palette, indices) = result.remapped(&mut image)?;
    let elapsed = start.elapsed();

    write_png(&opts.output, width, height, &palette, &indices)?;
    println!(
        "{width}x{height}: {} colors, quality {}, {:.1} ms",
        palette.len(),
        result.quantization_quality().map_or("?".into(), |q| q.to_string()),
        elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}
