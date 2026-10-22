//! Converts PNG frames to an animated GIF with one palette for all frames.
//!
//! ```text
//! cargo run --release --features png --example animated_gif -- output.gif frame1.png frame2.png ...
//!     [--delay CENTISECONDS] [--colors N] [--speed N] [--nofs] [--compare]
//! cargo run --release --features png --example animated_gif [output.gif]   # a synthetic animation
//! ```
//!
//! The colors of all frames go into one [`Histogram`], so the palette also covers colors
//! that appear in only a few frames, and it is written once, as the GIF's global color
//! table. Frames of different sizes are cropped to the smallest width and height, keeping
//! their centers.
//!
//! GIF has no partial transparency, only one transparent palette index: pixels with alpha
//! below 128 become fully transparent and all others fully opaque before quantizing, so
//! soft edges (shadows, antialiasing against transparency) become hard. Blend such frames
//! onto a background color first if that matters. The transparent color is added to the
//! histogram as a fixed color and put at the last index (`set_last_index_transparent`).
//!
//! `--compare` also prints each frame's PSNR with the shared palette, with the first frame's
//! palette and with a palette of its own, and the size of a GIF with a local color table per
//! frame.

use std::error::Error;

use libimgquant::{Attributes, Histogram, RGBA};

struct Options {
    output: String,
    inputs: Vec<String>,
    delay: u16,
    colors: u32,
    speed: i32,
    dither: f32,
    compare: bool,
}

const USAGE: &str =
    "usage: animated_gif [output.gif [frame.png...]] [--delay CS] [--colors N] [--speed N] [--nofs] [--compare]";

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut opts = Options {
        output: String::new(),
        inputs: Vec::new(),
        delay: 8,
        colors: 256,
        speed: 3,
        dither: 1.0,
        compare: false,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--delay" => opts.delay = value("--delay")?.parse().map_err(|e| format!("--delay: {e}"))?,
            "--colors" => opts.colors = value("--colors")?.parse().map_err(|e| format!("--colors: {e}"))?,
            "--speed" => opts.speed = value("--speed")?.parse().map_err(|e| format!("--speed: {e}"))?,
            "--nofs" => opts.dither = 0.0,
            "--compare" => opts.compare = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    opts.output = positional.next().unwrap_or_else(|| "animated.gif".into());
    opts.inputs = positional.collect();
    Ok(opts)
}

/// A frame as RGBA pixels.
struct Frame {
    pixels: Vec<RGBA>,
    width: usize,
    height: usize,
}

impl Frame {
    /// The `width` x `height` rectangle in the middle of the frame.
    fn crop_center(&self, width: usize, height: usize) -> Vec<RGBA> {
        let (x0, y0) = ((self.width - width) / 2, (self.height - height) / 2);
        (y0..y0 + height)
            .flat_map(|y| &self.pixels[y * self.width + x0..][..width])
            .copied()
            .collect()
    }
}

/// Frames when none are given: 24 frames of a plasma with a transparent hole moving across it.
fn synthetic_frames() -> Vec<Frame> {
    let (width, height) = (160, 120);
    (0..24)
        .map(|t| {
            let phase = f64::from(t) / 24.0 * std::f64::consts::TAU;
            let pixels = (0..width * height)
                .map(|i| {
                    let (x, y) = ((i % width) as f64 / 20.0, (i / width) as f64 / 20.0);
                    let hole = ((x - 4.0 - 2.5 * phase.cos()).powi(2) + (y - 3.0).powi(2)).sqrt() < 0.9;
                    let v = (x + phase).sin() + (y * 1.3 - phase).cos() + ((x + y) * 0.7 + phase * 2.0).sin();
                    let channel = |shift: f64| ((v * 0.9 + shift).sin() * 0.5 + 0.5) * 255.0;
                    let a = if hole { 0 } else { 255 };
                    RGBA::new(channel(0.0) as u8, channel(2.1) as u8, channel(4.2) as u8, a)
                })
                .collect();
            Frame { pixels, width, height }
        })
        .collect()
}

/// A remapped frame: its palette, its indices and the transparent index.
type Remapped = (Vec<RGBA>, Vec<u8>, Option<u8>);

/// Peak signal-to-noise ratio of the remapped frame, over premultiplied RGB and alpha.
fn psnr(pixels: &[RGBA], palette: &[RGBA], indices: &[u8]) -> f64 {
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
    let mse = sum / (pixels.len() * 4) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Remaps a frame and makes sure every fully transparent pixel gets the transparent index:
/// dithering can move a transparent pixel to a visible color.
fn remap(
    result: &mut libimgquant::QuantizationResult,
    attr: &Attributes,
    pixels: &[RGBA],
    width: usize,
    height: usize,
) -> Result<Remapped, libimgquant::Error> {
    let mut image = attr.new_image_borrowed(pixels, width, height, 0.0)?;
    let (palette, mut indices) = result.remapped(&mut image)?;
    let transparent = palette.iter().position(|c| c.a == 0);
    if let Some(t) = transparent {
        for (index, px) in indices.iter_mut().zip(pixels) {
            if px.a == 0 {
                *index = t as u8;
            }
        }
    }
    Ok((palette, indices, transparent.map(|t| t as u8)))
}

/// Encodes the frames; `palette` is the global color table, or each frame brings its own.
fn encode_gif(
    width: u16,
    height: u16,
    global_palette: Option<&[RGBA]>,
    frames: &[Remapped],
    delay: u16,
) -> Result<Vec<u8>, gif::EncodingError> {
    let rgb = |palette: &[RGBA]| -> Vec<u8> { palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect() };
    let mut out = Vec::new();
    {
        let global = global_palette.map(rgb).unwrap_or_default();
        let mut encoder = gif::Encoder::new(&mut out, width, height, &global)?;
        encoder.set_repeat(gif::Repeat::Infinite)?;
        for (palette, indices, transparent) in frames {
            let frame = gif::Frame {
                width,
                height,
                delay,
                transparent: *transparent,
                // frames cover the whole canvas; clearing lets transparent pixels show the
                // background instead of the previous frame
                dispose: gif::DisposalMethod::Background,
                palette: if global_palette.is_some() {
                    None
                } else {
                    Some(rgb(palette))
                },
                buffer: indices.as_slice().into(),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame)?;
        }
    }
    Ok(out)
}

fn main() -> Result<(), Box<dyn Error>> {
    let opts = parse_args().inspect_err(|_| eprintln!("{USAGE}"))?;

    let frames = if opts.inputs.is_empty() {
        synthetic_frames()
    } else {
        let mut frames = Vec::new();
        for path in &opts.inputs {
            let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let decoded = libimgquant::png_io::decode(&data).map_err(|e| format!("{path}: {e}"))?;
            frames.push(Frame {
                pixels: decoded.pixels,
                width: decoded.width,
                height: decoded.height,
            });
        }
        frames
    };
    let width = frames.iter().map(|f| f.width).min().ok_or("no frames")?;
    let height = frames.iter().map(|f| f.height).min().ok_or("no frames")?;
    let (gif_width, gif_height) = (u16::try_from(width)?, u16::try_from(height)?);

    // one transparent color: alpha below 128 is transparent, the rest opaque
    let frames: Vec<Vec<RGBA>> = frames
        .iter()
        .map(|f| {
            let mut pixels = f.crop_center(width, height);
            for px in &mut pixels {
                *px = if px.a < 128 {
                    RGBA::new(0, 0, 0, 0)
                } else {
                    RGBA { a: 255, ..*px }
                };
            }
            pixels
        })
        .collect();
    let has_transparency = frames.iter().flatten().any(|px| px.a == 0);

    let mut attr = libimgquant::new();
    attr.set_speed(opts.speed)?;
    attr.set_max_colors(opts.colors)?;
    attr.set_last_index_transparent(true);

    let mut histogram = Histogram::new(&attr);
    for pixels in &frames {
        let mut image = attr.new_image_borrowed(pixels, width, height, 0.0)?;
        histogram.add_image(&attr, &mut image)?;
    }
    if has_transparency {
        // exactly one palette entry for all transparent pixels
        histogram.add_fixed_color(RGBA::new(0, 0, 0, 0), 0.0)?;
    }
    let mut result = histogram.quantize(&attr)?;
    result.set_dithering_level(opts.dither)?;

    let remapped = frames
        .iter()
        .map(|pixels| remap(&mut result, &attr, pixels, width, height))
        .collect::<Result<Vec<_>, _>>()?;
    let palette = result.palette_vec();
    let gif = encode_gif(gif_width, gif_height, Some(&palette), &remapped, opts.delay)?;
    std::fs::write(&opts.output, &gif)?;
    println!(
        "{} frames, {width}x{height}, {} colors{}: {} bytes written to {}",
        frames.len(),
        palette.len(),
        match remapped[0].2 {
            Some(t) => format!(" (index {t} transparent)"),
            None => String::new(),
        },
        gif.len(),
        opts.output
    );

    if opts.compare {
        compare(&attr, &opts, &frames, (width, height), &remapped, gif.len())?;
    }
    Ok(())
}

/// Prints the PSNR of each frame with the shared palette, with the palette of the first
/// frame (the simple alternative), and with a palette of its own; and the size of a GIF
/// with a palette per frame.
fn compare(
    attr: &Attributes,
    opts: &Options,
    frames: &[Vec<RGBA>],
    (width, height): (usize, usize),
    shared: &[Remapped],
    shared_size: usize,
) -> Result<(), Box<dyn Error>> {
    let quantize = |pixels: &[RGBA]| -> Result<libimgquant::QuantizationResult, libimgquant::Error> {
        let mut image = attr.new_image_borrowed(pixels, width, height, 0.0)?;
        let mut result = attr.quantize(&mut image)?;
        result.set_dithering_level(opts.dither)?;
        Ok(result)
    };
    let mut first = quantize(&frames[0])?;
    let mut own_frames = Vec::new();
    let mut sums = [0.0; 3];
    println!("frame  shared palette  first frame's  own palette  own quality");
    for (i, (pixels, (palette, indices, _))) in frames.iter().zip(shared).enumerate() {
        let mut own = quantize(pixels)?;
        let own_frame = remap(&mut own, attr, pixels, width, height)?;
        let first_frame = remap(&mut first, attr, pixels, width, height)?;
        let psnrs = [
            psnr(pixels, palette, indices),
            psnr(pixels, &first_frame.0, &first_frame.1),
            psnr(pixels, &own_frame.0, &own_frame.1),
        ];
        println!(
            "{i:5}  {:11.2} dB  {:10.2} dB  {:8.2} dB  {:>11}",
            psnrs[0],
            psnrs[1],
            psnrs[2],
            own.quantization_quality().map_or("?".into(), |q| q.to_string())
        );
        for (sum, p) in sums.iter_mut().zip(psnrs) {
            *sum += p;
        }
        own_frames.push(own_frame);
    }
    let n = frames.len() as f64;
    println!(
        " mean  {:11.2} dB  {:10.2} dB  {:8.2} dB",
        sums[0] / n,
        sums[1] / n,
        sums[2] / n
    );
    let local = encode_gif(width as u16, height as u16, None, &own_frames, opts.delay)?;
    println!(
        "GIF size: {shared_size} bytes with one palette, {} bytes with a palette per frame",
        local.len()
    );
    Ok(())
}
