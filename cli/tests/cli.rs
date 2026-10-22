//! Runs the built `imgquant` binary on PNGs generated here.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_imgquant");

/// A fresh, empty directory for one test.
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("imgquant-cli").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN).current_dir(dir).args(args).output().unwrap()
}

fn run_stdin(dir: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exited normally")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Encodes a PNG with the given format; `setup` can add chunks.
fn encode(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
    setup: impl FnOnce(&mut png::Encoder<'_, &mut Vec<u8>>),
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    setup(&mut encoder);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(data).unwrap();
    writer.finish().unwrap();
    out
}

/// A 64x48 RGBA image: smooth gradients with a transparent corner and a translucent band.
fn rgba_pixels() -> (u32, u32, Vec<[u8; 4]>) {
    let (w, h) = (64u32, 48u32);
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let a = if x < 8 && y < 8 {
                0
            } else if (20..28).contains(&y) {
                128
            } else {
                255
            };
            pixels.push([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8, a]);
        }
    }
    (w, h, pixels)
}

fn rgba_png() -> Vec<u8> {
    let (w, h, pixels) = rgba_pixels();
    encode(
        w,
        h,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        pixels.as_flattened(),
        |_| {},
    )
}

/// Pseudo-random RGB noise, which needs many colors.
fn noise_png(w: u32, h: u32) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    let data: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    encode(w, h, png::ColorType::Rgb, png::BitDepth::Eight, &data, |_| {})
}

/// A decoded output file: indexed data checked, then expanded to RGBA.
struct Indexed {
    width: u32,
    height: u32,
    bit_depth: u8,
    palette_len: usize,
    rgba: Vec<[u8; 4]>,
    gama: Option<u32>,
}

fn decode_indexed(data: &[u8]) -> Indexed {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut buf).unwrap();
    assert_eq!(frame.color_type, png::ColorType::Indexed);
    let info = reader.info();
    let palette = info.palette.as_ref().unwrap();
    let trns = info.trns.as_deref().unwrap_or(&[]);
    let bits = frame.bit_depth as usize;
    let stride = frame.line_size;
    let mut rgba = Vec::new();
    for y in 0..frame.height as usize {
        for x in 0..frame.width as usize {
            let bit = x * bits;
            let byte = buf[y * stride + bit / 8];
            let index = usize::from((byte >> (8 - bits - bit % 8)) & ((1u16 << bits) - 1) as u8);
            let p = &palette[index * 3..index * 3 + 3];
            rgba.push([p[0], p[1], p[2], trns.get(index).copied().unwrap_or(255)]);
        }
    }
    Indexed {
        width: frame.width,
        height: frame.height,
        bit_depth: bits as u8,
        palette_len: palette.len() / 3,
        rgba,
        gama: info.gama_chunk.map(png::ScaledFloat::into_scaled),
    }
}

/// The color of fully transparent pixels doesn't matter.
fn visible(pixels: &[[u8; 4]]) -> Vec<[u8; 4]> {
    pixels.iter().map(|&p| if p[3] == 0 { [0; 4] } else { p }).collect()
}

fn max_channel_error(a: &[[u8; 4]], b: &[[u8; 4]]) -> u8 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| (0..4).map(move |c| p[c].abs_diff(q[c])))
        .max()
        .unwrap()
}

#[test]
fn help_and_version() {
    let dir = test_dir("help");
    let out = run(&dir, &["--help"]);
    assert_eq!(code(&out), 0);
    let help = String::from_utf8(out.stdout).unwrap();
    for text in ["--quality", "--skip-if-larger", "98", "99", "15", "--ext"] {
        assert!(help.contains(text), "help mentions {text}");
    }
    let out = run(&dir, &["-V"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        concat!("imgquant ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_2() {
    let dir = test_dir("usage");
    fs::write(dir.join("a.png"), rgba_png()).unwrap();
    for args in [
        &[][..],
        &["1", "a.png"],
        &["300", "a.png"],
        &["--speed", "0", "a.png"],
        &["--quality", "90-50", "a.png"],
        &["--quality", "x", "a.png"],
        &["--posterize", "9", "a.png"],
        &["--floyd=1.5", "a.png"],
        &["--profile", "best", "a.png"],
        &["--frobnicate", "a.png"],
        &["-o", "x.png", "a.png", "a.png"],
        &["-", "a.png"],
    ] {
        let out = run(&dir, args);
        assert_eq!(code(&out), 2, "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("--help"), "{args:?}");
    }
    // nothing was written
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn output_names_and_overwriting() {
    let dir = test_dir("naming");
    fs::write(dir.join("a.png"), rgba_png()).unwrap();

    assert_eq!(code(&run(&dir, &["a.png"])), 0);
    assert!(dir.join("a-fs8.png").exists());
    assert_eq!(code(&run(&dir, &["--nofs", "a.png"])), 0);
    assert!(dir.join("a-or8.png").exists());
    assert_eq!(code(&run(&dir, &["--ext", ".small.png", "a.png"])), 0);
    assert!(dir.join("a.small.png").exists());
    assert_eq!(code(&run(&dir, &["-o", "sub-out.png", "a.png"])), 0);
    assert!(dir.join("sub-out.png").exists());

    // existing output: 15 without --force, the file unchanged
    fs::write(dir.join("a-fs8.png"), b"keep").unwrap();
    let out = run(&dir, &["a.png"]);
    assert_eq!(code(&out), 15);
    assert!(stderr(&out).contains("--force"));
    assert_eq!(fs::read(dir.join("a-fs8.png")).unwrap(), b"keep");
    // -q hides the notice
    let out = run(&dir, &["-q", "a.png"]);
    assert_eq!(code(&out), 15);
    assert!(out.stderr.is_empty());
    // --force replaces it
    assert_eq!(code(&run(&dir, &["-f", "a.png"])), 0);
    decode_indexed(&fs::read(dir.join("a-fs8.png")).unwrap());

    // in place with --ext .png --force
    fs::copy(dir.join("a.png"), dir.join("b.png")).unwrap();
    assert_eq!(code(&run(&dir, &["--ext", ".png", "b.png"])), 15);
    assert_eq!(code(&run(&dir, &["--ext", ".png", "--force", "b.png"])), 0);
    decode_indexed(&fs::read(dir.join("b.png")).unwrap());

    // no temporary files are left behind
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["a-fs8.png", "a-or8.png", "a.png", "a.small.png", "b.png", "sub-out.png"]
    );
}

#[test]
fn several_files_continue_after_errors() {
    let dir = test_dir("several");
    fs::write(dir.join("a.png"), rgba_png()).unwrap();
    fs::write(dir.join("bad.png"), b"not a png").unwrap();
    fs::write(dir.join("c.png"), rgba_png()).unwrap();
    // the first failure decides the exit code; the other files are still converted
    let out = run(&dir, &["bad.png", "a.png", "missing.png", "c.png"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(stderr(&out).contains("bad.png"));
    assert!(stderr(&out).contains("missing.png"));
    assert!(dir.join("a-fs8.png").exists());
    assert!(dir.join("c-fs8.png").exists());
    assert!(!dir.join("bad-fs8.png").exists());

    // a 15 before a 1 gives 15
    let out = run(&dir, &["a.png", "missing.png"]);
    assert_eq!(code(&out), 15);
}

#[test]
fn skip_if_larger() {
    let dir = test_dir("skip");
    // a 1x1 one-bit gray PNG is smaller than any palette PNG
    let tiny = encode(1, 1, png::ColorType::Grayscale, png::BitDepth::One, &[0x80], |_| {});
    fs::write(dir.join("tiny.png"), &tiny).unwrap();
    let out = run(&dir, &["--skip-if-larger", "tiny.png"]);
    assert_eq!(code(&out), 99, "{}", stderr(&out));
    assert!(!dir.join("tiny-fs8.png").exists());
    // without the option it is written
    assert_eq!(code(&run(&dir, &["tiny.png"])), 0);
    assert!(dir.join("tiny-fs8.png").exists());

    // a 24-bit gradient gets much smaller
    let (w, h, pixels) = rgba_pixels();
    let rgb: Vec<u8> = pixels.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let big = encode(w, h, png::ColorType::Rgb, png::BitDepth::Eight, &rgb, |e| {
        e.set_compression(png::Compression::NoCompression);
    });
    fs::write(dir.join("big.png"), &big).unwrap();
    assert_eq!(code(&run(&dir, &["--skip-if-larger", "big.png"])), 0);
    assert!(fs::metadata(dir.join("big-fs8.png")).unwrap().len() < big.len() as u64);
}

#[test]
fn quality_too_low_exits_98() {
    let dir = test_dir("quality");
    fs::write(dir.join("noise.png"), noise_png(64, 64)).unwrap();
    let out = run(&dir, &["--quality", "95-100", "8", "noise.png"]);
    assert_eq!(code(&out), 98, "{}", stderr(&out));
    assert!(stderr(&out).contains("quality"));
    assert!(!dir.join("noise-fs8.png").exists());
    // a reachable quality is fine
    assert_eq!(code(&run(&dir, &["--quality", "0-50", "noise.png"])), 0);
    assert!(dir.join("noise-fs8.png").exists());
}

#[test]
fn stdin_to_stdout() {
    let dir = test_dir("stdio");
    let out = run_stdin(&dir, &["16", "-"], &rgba_png());
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let image = decode_indexed(&out.stdout);
    assert!(image.palette_len <= 16);
    assert_eq!((image.width, image.height), (64, 48));

    // -o with standard input
    let out = run_stdin(&dir, &["-o", "out.png", "-"], &rgba_png());
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    decode_indexed(&fs::read(dir.join("out.png")).unwrap());

    // garbage in: exit 1, nothing on stdout
    let out = run_stdin(&dir, &["-"], b"garbage");
    assert_eq!(code(&out), 1);
    assert!(out.stdout.is_empty());
}

#[test]
fn round_trip_keeps_alpha_and_color_limit() {
    let dir = test_dir("roundtrip");
    fs::write(dir.join("a.png"), rgba_png()).unwrap();
    let (_, _, original) = rgba_pixels();
    for (colors, depth) in [("2", 1), ("4", 2), ("16", 4), ("200", 8)] {
        let out = run(&dir, &["-f", "--nofs", colors, "-o", "out.png", "a.png"]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let image = decode_indexed(&fs::read(dir.join("out.png")).unwrap());
        assert!(image.palette_len <= colors.parse().unwrap(), "{colors}");
        assert_eq!(
            image.bit_depth, depth,
            "smallest bit depth for {} colors",
            image.palette_len
        );
        assert_eq!(image.rgba.len(), original.len());
        if image.palette_len < 16 {
            // too few colors to keep every alpha level
            continue;
        }
        for (p, q) in original.iter().zip(&image.rgba) {
            if p[3] == 0 {
                assert_eq!(q[3], 0, "transparent stays transparent");
            } else if p[3] == 255 {
                assert_eq!(q[3], 255, "opaque stays opaque");
            }
        }
        if colors == "200" {
            assert!(max_channel_error(&visible(&original), &visible(&image.rgba)) <= 24);
        }
    }
}

#[test]
fn gray_16bit_and_palette_inputs() {
    let dir = test_dir("formats");
    let (w, h) = (40u32, 30u32);
    let gray: Vec<u8> = (0..w * h).map(|i| ((i % w) * 6) as u8).collect();
    let expected_gray: Vec<[u8; 4]> = gray.iter().map(|&g| [g, g, g, 255]).collect();

    // 8-bit gray with a tRNS color (gray 0 is transparent)
    let gray_trns = encode(w, h, png::ColorType::Grayscale, png::BitDepth::Eight, &gray, |e| {
        e.set_trns(vec![0, 0]);
    });
    // 16-bit gray: high bytes are the gray values
    let gray16: Vec<u8> = gray.iter().flat_map(|&g| [g, 0x7f]).collect();
    let gray16 = encode(w, h, png::ColorType::Grayscale, png::BitDepth::Sixteen, &gray16, |_| {});
    // 16-bit RGBA
    let (rw, rh, rgba) = rgba_pixels();
    let rgba16: Vec<u8> = rgba.iter().flat_map(|p| p.iter().flat_map(|&c| [c, c])).collect();
    let rgba16 = encode(rw, rh, png::ColorType::Rgba, png::BitDepth::Sixteen, &rgba16, |_| {});
    // 4-bit palette with tRNS
    let palette: Vec<u8> = (0..16u8).flat_map(|i| [i * 16, 255 - i * 16, i * 8]).collect();
    let alpha: Vec<u8> = (0..16u8).map(|i| if i < 2 { i * 100 } else { 255 }).collect();
    let indices: Vec<u8> = (0..w * h).map(|i| ((i % w) / 3 % 16) as u8).collect();
    let packed: Vec<u8> = indices.chunks(2).map(|p| p[0] << 4 | p[1]).collect();
    let indexed = encode(w, h, png::ColorType::Indexed, png::BitDepth::Four, &packed, |e| {
        e.set_palette(palette.clone());
        e.set_trns(alpha.clone());
    });
    let expected_indexed: Vec<[u8; 4]> = indices
        .iter()
        .map(|&i| {
            let i = usize::from(i);
            [palette[i * 3], palette[i * 3 + 1], palette[i * 3 + 2], alpha[i]]
        })
        .collect();

    for (name, data) in [
        ("gray-trns.png", &gray_trns),
        ("gray16.png", &gray16),
        ("rgba16.png", &rgba16),
        ("indexed.png", &indexed),
    ] {
        fs::write(dir.join(name), data).unwrap();
    }
    let out = run(&dir, &["gray-trns.png", "gray16.png", "rgba16.png", "indexed.png"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let read = |name: &str| decode_indexed(&fs::read(dir.join(name)).unwrap());
    // few distinct colors: exact
    let image = read("gray16-fs8.png");
    assert_eq!(image.rgba, expected_gray);
    let image = read("indexed-fs8.png");
    assert_eq!(visible(&image.rgba), visible(&expected_indexed));
    assert!(image.palette_len <= 16);
    let image = read("gray-trns-fs8.png");
    for (p, q) in expected_gray.iter().zip(&image.rgba) {
        assert_eq!(q[3], if p[0] == 0 { 0 } else { 255 });
        if p[0] != 0 {
            assert_eq!(p, q);
        }
    }
    let image = read("rgba16-fs8.png");
    assert!(max_channel_error(&visible(&rgba), &visible(&image.rgba)) <= 32);
}

#[test]
fn gamma_chunk_is_kept() {
    let dir = test_dir("gamma");
    let (w, h, pixels) = rgba_pixels();
    let with_gama = encode(
        w,
        h,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        pixels.as_flattened(),
        |e| {
            e.set_source_gamma(png::ScaledFloat::from_scaled(55_000));
        },
    );
    let with_srgb = encode(
        w,
        h,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        pixels.as_flattened(),
        |e| {
            e.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        },
    );
    fs::write(dir.join("gama.png"), with_gama).unwrap();
    fs::write(dir.join("srgb.png"), with_srgb).unwrap();
    fs::write(dir.join("plain.png"), rgba_png()).unwrap();
    let out = run(&dir, &["-v", "gama.png", "srgb.png", "plain.png"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let read = |name: &str| decode_indexed(&fs::read(dir.join(name)).unwrap());
    assert_eq!(read("gama-fs8.png").gama, Some(55_000));
    assert_eq!(read("srgb-fs8.png").gama, None);
    assert_eq!(read("plain-fs8.png").gama, None);
}

#[test]
fn verbose_prints_statistics() {
    let dir = test_dir("verbose");
    fs::write(dir.join("a.png"), rgba_png()).unwrap();
    let out = run(&dir, &["--verbose", "--speed", "1", "--profile", "compat", "a.png"]);
    assert_eq!(code(&out), 0);
    let log = stderr(&out);
    for text in [
        "a.png:",
        "64x48",
        "colors, quality",
        "wrote",
        "% of the input",
        "ms in total",
    ] {
        assert!(log.contains(text), "{text} in {log}");
    }
    // the quantizer's own log
    assert!(log.contains("histogram"), "{log}");
    // quiet by default
    let out = run(&dir, &["-f", "a.png"]);
    assert!(out.stderr.is_empty());
}
