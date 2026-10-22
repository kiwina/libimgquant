//! Quality and speed evaluation over a directory of PNG images.
//!
//! ```text
//! cargo run --release --example evaluate -- <dir> [--speed N] [--dither F] [--colors N]
//!     [--quality MIN-MAX] [--profile compat|improved] [--repeat N] [--save results.tsv] [--compare baseline.tsv]
//! ```
//!
//! Prints per-image metrics and totals. `--save` writes them as TSV; `--compare` prints
//! the change relative to a previously saved run.

mod metrics;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use libimgquant::RGBA;

struct Config {
    dir: PathBuf,
    speed: i32,
    dither: f32,
    colors: u32,
    quality: Option<(u8, u8)>,
    profile: libimgquant::Profile,
    repeat: usize,
    save: Option<PathBuf>,
    compare: Option<PathBuf>,
}

#[derive(Clone)]
struct Row {
    name: String,
    pixels: usize,
    colors: usize,
    ms: f64,
    psnr: f64,
    dssim: f64,
    delta_e: f64,
    delta_e_p99: f64,
    png_bytes: usize,
}

const COLUMNS: &str = "image\tpixels\tcolors\tms\tpsnr\tdssim_x1000\tdeltaE\tdeltaE_p99\tpng_bytes";

impl Row {
    fn tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{:.1}\t{:.3}\t{:.4}\t{:.4}\t{:.3}\t{}",
            self.name,
            self.pixels,
            self.colors,
            self.ms,
            self.psnr,
            self.dssim * 1000.0,
            self.delta_e,
            self.delta_e_p99,
            self.png_bytes
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 9 || f[0] == "image" {
            return None;
        }
        Some(Self {
            name: f[0].to_string(),
            pixels: f[1].parse().ok()?,
            colors: f[2].parse().ok()?,
            ms: f[3].parse().ok()?,
            psnr: f[4].parse().ok()?,
            dssim: f[5].parse::<f64>().ok()? / 1000.0,
            delta_e: f[6].parse().ok()?,
            delta_e_p99: f[7].parse().ok()?,
            png_bytes: f[8].parse().ok()?,
        })
    }
}

fn parse_args() -> Result<Config, String> {
    let mut args = std::env::args().skip(1);
    let mut cfg = Config {
        dir: PathBuf::new(),
        speed: 3,
        dither: 1.0,
        colors: 256,
        quality: None,
        profile: libimgquant::Profile::Compat,
        repeat: 1,
        save: None,
        compare: None,
    };
    let mut dir = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--profile" => {
                cfg.profile = match value("--profile")?.as_str() {
                    "compat" => libimgquant::Profile::Compat,
                    "improved" => libimgquant::Profile::Improved,
                    other => return Err(format!("unknown profile {other}")),
                }
            }
            "--speed" => cfg.speed = value("--speed")?.parse().map_err(|e| format!("--speed: {e}"))?,
            "--dither" => cfg.dither = value("--dither")?.parse().map_err(|e| format!("--dither: {e}"))?,
            "--colors" => cfg.colors = value("--colors")?.parse().map_err(|e| format!("--colors: {e}"))?,
            "--repeat" => cfg.repeat = value("--repeat")?.parse().map_err(|e| format!("--repeat: {e}"))?,
            "--quality" => {
                let v = value("--quality")?;
                let (a, b) = v.split_once('-').ok_or("--quality takes MIN-MAX")?;
                cfg.quality = Some((
                    a.parse().map_err(|_| "bad quality")?,
                    b.parse().map_err(|_| "bad quality")?,
                ));
            }
            "--save" => cfg.save = Some(value("--save")?.into()),
            "--compare" => cfg.compare = Some(value("--compare")?.into()),
            _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    cfg.dir = dir.ok_or("missing image directory")?;
    Ok(cfg)
}

fn read_png(path: &Path) -> Result<(Vec<RGBA>, usize, usize), Box<dyn std::error::Error>> {
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

fn png_size(width: usize, height: usize, palette: &[RGBA], indices: &[u8]) -> usize {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    encoder.set_filter(png::Filter::NoFilter);
    encoder.set_palette(palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect::<Vec<_>>());
    let trns_len = palette.iter().rposition(|c| c.a != 255).map_or(0, |i| i + 1);
    if trns_len > 0 {
        encoder.set_trns(palette[..trns_len].iter().map(|c| c.a).collect::<Vec<_>>());
    }
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(indices).expect("png data");
    writer.finish().expect("png finish");
    out.len()
}

fn evaluate(cfg: &Config, path: &Path) -> Result<Row, Box<dyn std::error::Error>> {
    let (pixels, width, height) = read_png(path)?;
    let mut attr = libimgquant::new();
    attr.set_profile(cfg.profile);
    attr.set_speed(cfg.speed)?;
    attr.set_max_colors(cfg.colors)?;
    if let Some((min, max)) = cfg.quality {
        attr.set_quality(min, max)?;
    }

    let mut best_ms = f64::MAX;
    let mut output = None;
    for _ in 0..cfg.repeat.max(1) {
        let start = Instant::now();
        let mut image = attr.new_image_borrowed(&pixels, width, height, 0.0)?;
        let mut res = attr.quantize(&mut image)?;
        res.set_dithering_level(cfg.dither)?;
        let out = res.remapped(&mut image)?;
        best_ms = best_ms.min(start.elapsed().as_secs_f64() * 1000.0);
        output = Some(out);
    }
    let (palette, indices) = output.expect("ran at least once");
    let m = metrics::measure(&pixels, &palette, &indices, width, height);
    Ok(Row {
        name: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        pixels: width * height,
        colors: palette.len(),
        ms: best_ms,
        psnr: m.psnr,
        dssim: m.dssim,
        delta_e: m.delta_e,
        delta_e_p99: m.delta_e_p99,
        png_bytes: png_size(width, height, &palette, &indices),
    })
}

fn totals(rows: &[Row]) -> Row {
    let n = rows.len().max(1) as f64;
    Row {
        name: "TOTAL/MEAN".into(),
        pixels: rows.iter().map(|r| r.pixels).sum(),
        colors: rows.iter().map(|r| r.colors).sum(),
        ms: rows.iter().map(|r| r.ms).sum(),
        psnr: rows.iter().map(|r| r.psnr).sum::<f64>() / n,
        dssim: rows.iter().map(|r| r.dssim).sum::<f64>() / n,
        delta_e: rows.iter().map(|r| r.delta_e).sum::<f64>() / n,
        delta_e_p99: rows.iter().map(|r| r.delta_e_p99).sum::<f64>() / n,
        png_bytes: rows.iter().map(|r| r.png_bytes).sum(),
    }
}

fn pct(new: f64, old: f64) -> String {
    if old == 0.0 {
        "n/a".into()
    } else {
        format!("{:+.2}%", (new / old - 1.0) * 100.0)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_args().inspect_err(|_| {
        eprintln!(
            "usage: evaluate <dir> [--speed N] [--dither F] [--colors N] [--quality MIN-MAX] [--profile compat|improved] [--repeat N] [--save f.tsv] [--compare f.tsv]"
        );
    })?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&cfg.dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .collect();
    paths.sort();

    println!("{COLUMNS}");
    let mut rows = Vec::new();
    for path in &paths {
        match evaluate(&cfg, path) {
            Ok(row) => {
                println!("{}", row.tsv());
                rows.push(row);
            }
            Err(e) => println!("{}\tERROR {e}", path.display()),
        }
    }
    let total = totals(&rows);
    println!("{}", total.tsv());

    if let Some(save) = &cfg.save {
        let mut f = File::create(save)?;
        writeln!(f, "{COLUMNS}")?;
        for row in rows.iter().chain([&total]) {
            writeln!(f, "{}", row.tsv())?;
        }
    }

    if let Some(compare) = &cfg.compare {
        let old: BTreeMap<String, Row> = std::fs::read_to_string(compare)?
            .lines()
            .filter_map(Row::parse)
            .map(|r| (r.name.clone(), r))
            .collect();
        println!(
            "\nchange vs {} (psnr in dB; others relative; lower dssim/deltaE/bytes/ms is better)",
            compare.display()
        );
        println!("image\tpsnr\tdssim\tdeltaE\tdeltaE_p99\tpng_bytes\tms");
        for row in rows.iter().chain([&total]) {
            if let Some(o) = old.get(&row.name) {
                println!(
                    "{}\t{:+.3}\t{}\t{}\t{}\t{}\t{}",
                    row.name,
                    row.psnr - o.psnr,
                    pct(row.dssim, o.dssim),
                    pct(row.delta_e, o.delta_e),
                    pct(row.delta_e_p99, o.delta_e_p99),
                    pct(row.png_bytes as f64, o.png_bytes as f64),
                    pct(row.ms, o.ms)
                );
            }
        }
    }
    Ok(())
}
