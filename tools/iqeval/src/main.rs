//! Quality, file-size and speed evaluation of libimgquant over a set of PNG images.
//!
//! See README.md for usage. In short:
//!
//! ```text
//! iqeval <dir-or-png>... [--profile compat|improved|both] [--speed N] [--dither F] [--colors N]
//!     [--quality MIN-MAX] [--repeat N] [--threads N] [--no-butteraugli] [--out DIR]
//!     [--save run.tsv] [--compare baseline.tsv]
//! iqeval --diff old.tsv new.tsv
//! ```

mod metrics;
mod pngout;
mod stats;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use libimgquant::{Profile, RGBA};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// `println!` that exits quietly when stdout is closed (for example piped into `head`).
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    };
}

const USAGE: &str = "usage: iqeval <dir-or-png>... [--profile compat|improved|both] [--speed N] [--dither F] \
[--colors N] [--quality MIN-MAX] [--repeat N] [--threads N] [--no-butteraugli] [--out DIR] [--save f.tsv] \
[--compare f.tsv]\n       iqeval --diff old.tsv new.tsv";

#[derive(Clone, Copy, PartialEq)]
enum Better {
    Higher,
    Lower,
    /// Informational column (pixel and color counts).
    Neither,
}

/// One numeric TSV column.
struct Column {
    name: &'static str,
    better: Better,
    /// Decimal places in the TSV. Comparisons use the rounded values, so differences
    /// below this precision count as ties.
    digits: usize,
    /// Show changes as a percentage (else as an absolute difference).
    relative: bool,
}

const fn col(name: &'static str, better: Better, digits: usize, relative: bool) -> Column {
    Column {
        name,
        better,
        digits,
        relative,
    }
}

const COLUMNS: [Column; 14] = [
    col("pixels", Better::Neither, 0, false),
    col("colors", Better::Neither, 0, false),
    col("ms", Better::Lower, 2, true),
    col("ssimu2", Better::Higher, 3, false),
    col("butter_p3", Better::Lower, 4, true),
    col("butter_max", Better::Lower, 3, true),
    col("de2000", Better::Lower, 4, true),
    col("deltaE", Better::Lower, 4, true),
    col("deltaE_p99", Better::Lower, 3, true),
    col("deltaE_blur", Better::Lower, 4, true),
    col("psnr", Better::Higher, 3, false),
    col("qerr", Better::Lower, 4, true),
    col("png_bytes", Better::Lower, 0, true),
    // bits per pixel of the PNG, so images of different sizes weigh the same in means
    col("bpp", Better::Lower, 4, true),
];

fn column(name: &str) -> usize {
    COLUMNS.iter().position(|c| c.name == name).expect("known column")
}

#[derive(Clone)]
struct Row {
    image: String,
    profile: String,
    values: [f64; COLUMNS.len()],
    /// FNV-1a hash of the palette and indices, to spot bit-identical outputs.
    hash: String,
}

impl Row {
    fn header() -> String {
        let mut s = String::from("image\tprofile");
        for c in &COLUMNS {
            s.push('\t');
            s.push_str(c.name);
        }
        s.push_str("\thash");
        s
    }

    fn tsv(&self) -> String {
        let mut s = format!("{}\t{}", self.image, self.profile);
        for (c, v) in COLUMNS.iter().zip(&self.values) {
            write!(s, "\t{v:.*}", c.digits).expect("write to string");
        }
        write!(s, "\t{}", self.hash).expect("write to string");
        s
    }

    fn get(&self, name: &str) -> f64 {
        self.values[column(name)]
    }
}

/// Reads a TSV written by `--save`. Columns are matched by name, so files from older or
/// newer versions load as long as `image` is present; missing metrics are NaN.
fn read_tsv(path: &Path) -> Result<Vec<Row>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_tsv(&text).map_err(|e| format!("{}: {e}", path.display()).into())
}

fn parse_tsv(text: &str) -> Result<Vec<Row>, String> {
    let mut header: Option<Vec<&str>> = None;
    let mut rows = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let fields: Vec<&str> = line.split('\t').collect();
        let Some(names) = &header else {
            if fields.first() != Some(&"image") {
                return Err("missing header line".into());
            }
            header = Some(fields);
            continue;
        };
        let field = |name: &str| {
            names
                .iter()
                .position(|n| *n == name)
                .and_then(|i| fields.get(i).copied())
        };
        let Some(image) = field("image") else { continue };
        if image == "MEAN" {
            continue;
        }
        let mut values = [f64::NAN; COLUMNS.len()];
        for (v, c) in values.iter_mut().zip(&COLUMNS) {
            if let Some(f) = field(c.name) {
                *v = f.parse().unwrap_or(f64::NAN);
            }
        }
        rows.push(Row {
            image: image.to_string(),
            profile: field("profile").unwrap_or("?").to_string(),
            values,
            hash: field("hash").unwrap_or("").to_string(),
        });
    }
    Ok(rows)
}

struct Config {
    inputs: Vec<PathBuf>,
    profiles: Vec<Profile>,
    speed: i32,
    dither: f32,
    colors: u32,
    quality: Option<(u8, u8)>,
    repeat: usize,
    threads: Option<usize>,
    butteraugli: bool,
    out_dir: Option<PathBuf>,
    save: Option<PathBuf>,
    compare: Option<PathBuf>,
    diff: Option<(PathBuf, PathBuf)>,
}

fn profile_name(p: Profile) -> &'static str {
    match p {
        Profile::Compat => "compat",
        Profile::Improved => "improved",
        _ => "other",
    }
}

fn parse_args() -> Result<Config, String> {
    let mut args = std::env::args().skip(1);
    let mut cfg = Config {
        inputs: Vec::new(),
        profiles: vec![Profile::Improved],
        speed: 3,
        dither: 1.0,
        colors: 256,
        quality: None,
        repeat: 1,
        threads: None,
        butteraugli: cfg!(feature = "butteraugli"),
        out_dir: None,
        save: None,
        compare: None,
        diff: None,
    };
    fn num<T: std::str::FromStr>(name: &str, v: String) -> Result<T, String>
    where
        T::Err: std::fmt::Display,
    {
        v.parse().map_err(|e| format!("{name}: {e}"))
    }
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--profile" => {
                cfg.profiles = match value("--profile")?.as_str() {
                    "compat" => vec![Profile::Compat],
                    "improved" => vec![Profile::Improved],
                    "both" => vec![Profile::Compat, Profile::Improved],
                    other => return Err(format!("unknown profile {other}")),
                }
            }
            "--speed" => cfg.speed = num("--speed", value("--speed")?)?,
            "--dither" => cfg.dither = num("--dither", value("--dither")?)?,
            "--colors" => cfg.colors = num("--colors", value("--colors")?)?,
            "--repeat" => cfg.repeat = num::<usize>("--repeat", value("--repeat")?)?.max(1),
            "--threads" => cfg.threads = Some(num("--threads", value("--threads")?)?),
            "--quality" => {
                let v = value("--quality")?;
                let (a, b) = v.split_once('-').ok_or("--quality takes MIN-MAX")?;
                cfg.quality = Some((num("--quality", a.into())?, num("--quality", b.into())?));
            }
            "--no-butteraugli" => cfg.butteraugli = false,
            "--out" => cfg.out_dir = Some(value("--out")?.into()),
            "--save" => cfg.save = Some(value("--save")?.into()),
            "--compare" => cfg.compare = Some(value("--compare")?.into()),
            "--diff" => {
                let old = value("--diff")?;
                cfg.diff = Some((old.into(), value("--diff")?.into()));
            }
            "-h" | "--help" => return Err(String::new()),
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ => cfg.inputs.push(arg.into()),
        }
    }
    if cfg.diff.is_none() && cfg.inputs.is_empty() {
        return Err("no images given".into());
    }
    Ok(cfg)
}

fn read_png(path: &Path) -> Result<(Vec<RGBA>, usize, usize), Error> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut buf)?;
    let bytes = &buf[..info.buffer_size()];
    let pixels = match info.color_type {
        png::ColorType::Rgba => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[r, g, b, a]| RGBA::new(r, g, b, a))
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[v, a]| RGBA::new(v, v, v, a))
            .collect(),
        other => return Err(format!("unexpected color type {other:?}").into()),
    };
    Ok((pixels, info.width as usize, info.height as usize))
}

fn list_images(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut paths = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(input)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
                .collect();
            found.sort();
            paths.extend(found);
        } else {
            paths.push(input.clone());
        }
    }
    Ok(paths)
}

fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

fn output_hash(palette: &[RGBA], indices: &[u8]) -> u64 {
    let pal: Vec<u8> = palette.iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
    fnv1a(fnv1a(0xcbf2_9ce4_8422_2325, &pal), indices)
}

struct Output {
    ms: f64,
    palette: Vec<RGBA>,
    indices: Vec<u8>,
    qerr: f64,
}

/// Quantizes and remaps once through the public API. The timed part covers image
/// creation, quantization and remapping (not PNG decoding or encoding).
fn quantize(cfg: &Config, profile: Profile, pixels: &[RGBA], width: usize, height: usize) -> Result<Output, Error> {
    let mut attr = libimgquant::new();
    attr.set_profile(profile);
    attr.set_speed(cfg.speed)?;
    attr.set_max_colors(cfg.colors)?;
    if let Some((min, max)) = cfg.quality {
        attr.set_quality(min, max)?;
    }
    let start = Instant::now();
    let mut image = attr.new_image_borrowed(pixels, width, height, 0.0)?;
    let mut res = attr.quantize(&mut image)?;
    res.set_dithering_level(cfg.dither)?;
    let (palette, indices) = res.remapped(&mut image)?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(Output {
        ms,
        palette,
        indices,
        qerr: res.quantization_error().unwrap_or(f64::NAN),
    })
}

/// Evaluates one image under every configured profile. Repetitions interleave the
/// profiles (A B A B ...), so machine load drifts affect them alike; the time is the
/// minimum over repetitions.
fn evaluate(cfg: &Config, pool: Option<&rayon::ThreadPool>, path: &Path) -> Result<Vec<Row>, Error> {
    let (pixels, width, height) = read_png(path)?;
    let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let mut outputs: Vec<Option<Output>> = cfg.profiles.iter().map(|_| None).collect();
    for _ in 0..cfg.repeat {
        for (slot, &profile) in outputs.iter_mut().zip(&cfg.profiles) {
            let run = || quantize(cfg, profile, &pixels, width, height);
            let out = match pool {
                Some(pool) => pool.install(run)?,
                None => run()?,
            };
            match slot {
                Some(prev) => {
                    if (&prev.palette, &prev.indices) != (&out.palette, &out.indices) {
                        eprintln!(
                            "warning: {name} ({}) output differs between runs",
                            profile_name(profile)
                        );
                    }
                    prev.ms = prev.ms.min(out.ms);
                }
                None => *slot = Some(out),
            }
        }
    }

    let opts = metrics::Options {
        butteraugli: cfg.butteraugli,
    };
    let mut rows = Vec::new();
    for (out, &profile) in outputs.into_iter().zip(&cfg.profiles) {
        let out = out.expect("ran at least once");
        let scores = metrics::measure(&pixels, &out.palette, &out.indices, width, height, opts);
        let png = pngout::encode_indexed(width, height, &out.palette, &out.indices)?;
        if let Some(dir) = &cfg.out_dir {
            std::fs::write(dir.join(format!("{name}.{}.png", profile_name(profile))), &png)?;
        }
        let pixels = (width * height) as f64;
        let values = [
            pixels,
            out.palette.len() as f64,
            out.ms,
            scores.ssimu2,
            scores.butter_p3,
            scores.butter_max,
            scores.de2000,
            scores.de_ok,
            scores.de_ok_p99,
            scores.de_ok_blur,
            scores.psnr,
            out.qerr,
            png.len() as f64,
            png.len() as f64 * 8.0 / pixels,
        ];
        let row = Row {
            image: name.clone(),
            profile: profile_name(profile).into(),
            values,
            hash: format!("{:016x}", output_hash(&out.palette, &out.indices)),
        };
        // round-trip through the TSV text, so fresh rows compare exactly like saved ones
        let header = Row::header();
        let rounded = parse_tsv(&format!("{header}\n{}", row.tsv()))?.pop().expect("one row");
        rows.push(rounded);
    }
    Ok(rows)
}

fn mean_row(rows: &[&Row], profile: &str) -> Row {
    let mut values = [f64::NAN; COLUMNS.len()];
    for (i, v) in values.iter_mut().enumerate() {
        let finite: Vec<f64> = rows.iter().map(|r| r.values[i]).filter(|v| v.is_finite()).collect();
        if !finite.is_empty() {
            *v = finite.iter().sum::<f64>() / finite.len() as f64;
        }
    }
    Row {
        image: "MEAN".into(),
        profile: profile.into(),
        values,
        hash: "-".into(),
    }
}

/// Rows of `rows` to compare against `profile`: that profile's rows if present, otherwise
/// all rows when the file holds a single profile.
fn select<'a>(rows: &'a [Row], profile: &str) -> Vec<&'a Row> {
    let matching: Vec<&Row> = rows.iter().filter(|r| r.profile == profile).collect();
    if !matching.is_empty() {
        return matching;
    }
    let profiles: BTreeSet<&str> = rows.iter().map(|r| r.profile.as_str()).collect();
    if profiles.len() == 1 {
        rows.iter().collect()
    } else {
        Vec::new()
    }
}

fn fmt_delta(c: &Column, old: f64, new: f64) -> String {
    if old == new {
        return "=".into();
    }
    if !old.is_finite() || !new.is_finite() {
        return "n/a".into();
    }
    if c.relative {
        if old == 0.0 {
            return "+inf%".into();
        }
        format!("{:+.2}%", (new / old - 1.0) * 100.0)
    } else {
        format!("{:+.*}", c.digits, new - old)
    }
}

fn class_of(image: &str) -> &str {
    image.split(['-', '_']).next().unwrap_or(image)
}

/// Prints paired per-image changes, then per-metric summaries and per-class summaries.
fn compare(old_rows: &[Row], new_rows: &[Row], title: &str) {
    let old: BTreeMap<&str, &Row> = old_rows.iter().map(|r| (r.image.as_str(), r)).collect();
    let pairs: Vec<(&Row, &Row)> = new_rows
        .iter()
        .filter_map(|n| old.get(n.image.as_str()).map(|o| (*o, n)))
        .collect();
    out!("\n== {title}: {} paired images", pairs.len());
    if pairs.is_empty() {
        return;
    }
    let metrics: Vec<&Column> = COLUMNS.iter().filter(|c| c.better != Better::Neither).collect();
    let mut header = String::from("image");
    for c in &metrics {
        write!(header, "\t{}", c.name).expect("write to string");
    }
    out!("{header}\tsame_output");
    for (o, n) in &pairs {
        let mut line = n.image.clone();
        for c in &metrics {
            write!(line, "\t{}", fmt_delta(c, o.get(c.name), n.get(c.name))).expect("write to string");
        }
        let same = !n.hash.is_empty() && n.hash == o.hash;
        out!("{line}\t{}", if same { "yes" } else { "no" });
    }
    let identical = pairs
        .iter()
        .filter(|(o, n)| !n.hash.is_empty() && n.hash == o.hash)
        .count();
    out!("identical outputs: {identical}/{}", pairs.len());

    out!(
        "\nmetric\tbetter\tmean_old\tmean_new\tchange\twins\tlosses\tties\twilcoxon_p\tgeomean_ratio  (wins = new better)"
    );
    for c in &metrics {
        let both: Vec<(f64, f64)> = pairs
            .iter()
            .map(|(o, n)| (o.get(c.name), n.get(c.name)))
            .filter(|(o, n)| o.is_finite() && n.is_finite())
            .collect();
        if both.is_empty() {
            out!("{}\t(no data)", c.name);
            continue;
        }
        let k = both.len() as f64;
        let mean_old = both.iter().map(|p| p.0).sum::<f64>() / k;
        let mean_new = both.iter().map(|p| p.1).sum::<f64>() / k;
        let sign = if c.better == Better::Higher { 1.0 } else { -1.0 };
        let wins = both.iter().filter(|(o, n)| (n - o) * sign > 0.0).count();
        let losses = both.iter().filter(|(o, n)| (n - o) * sign < 0.0).count();
        let diffs: Vec<f64> = both.iter().map(|(o, n)| n - o).collect();
        let p = stats::wilcoxon_signed_rank(&diffs).map_or("-".into(), |p| format!("{p:.4}"));
        let geo = if c.relative {
            stats::geomean_ratio(&both).map_or("-".into(), |g| format!("{g:.4}"))
        } else {
            "-".into()
        };
        out!(
            "{}\t{}\t{:.*}\t{:.*}\t{}\t{wins}\t{losses}\t{}\t{p}\t{geo}",
            c.name,
            if c.better == Better::Higher { "higher" } else { "lower" },
            c.digits + 1,
            mean_old,
            c.digits + 1,
            mean_new,
            fmt_delta(c, mean_old, mean_new),
            both.len() - wins - losses,
        );
    }

    let classes: BTreeSet<&str> = pairs.iter().map(|(_, n)| class_of(&n.image)).collect();
    if classes.len() > 1 {
        out!("\nclass\tn\tssimu2\tbutter_p3\tde2000\tdeltaE_blur\tpng_bytes(geo)\tms(geo)");
        for class in classes {
            let sub: Vec<&(&Row, &Row)> = pairs.iter().filter(|(_, n)| class_of(&n.image) == class).collect();
            let mean_change = |name: &str| {
                let c = &COLUMNS[column(name)];
                let both: Vec<(f64, f64)> = sub
                    .iter()
                    .map(|(o, n)| (o.get(name), n.get(name)))
                    .filter(|(o, n)| o.is_finite() && n.is_finite())
                    .collect();
                if both.is_empty() {
                    return "-".to_string();
                }
                let k = both.len() as f64;
                fmt_delta(
                    c,
                    both.iter().map(|p| p.0).sum::<f64>() / k,
                    both.iter().map(|p| p.1).sum::<f64>() / k,
                )
            };
            let geo = |name: &str| {
                let both: Vec<(f64, f64)> = sub.iter().map(|(o, n)| (o.get(name), n.get(name))).collect();
                stats::geomean_ratio(&both).map_or("-".into(), |g| format!("{:+.2}%", (g - 1.0) * 100.0))
            };
            out!(
                "{class}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                sub.len(),
                mean_change("ssimu2"),
                mean_change("butter_p3"),
                mean_change("de2000"),
                mean_change("deltaE_blur"),
                geo("png_bytes"),
                geo("ms"),
            );
        }
    }
}

fn settings_comment(cfg: &Config) -> String {
    format!(
        "# iqeval speed={} dither={} colors={} quality={} repeat={} threads={} lib_threads={} butteraugli={}",
        cfg.speed,
        cfg.dither,
        cfg.colors,
        cfg.quality.map_or("-".into(), |(a, b)| format!("{a}-{b}")),
        cfg.repeat,
        cfg.threads.map_or("default".into(), |t| t.to_string()),
        cfg!(feature = "threads"),
        cfg.butteraugli,
    )
}

fn main() -> Result<(), Error> {
    let cfg = parse_args().map_err(|e| {
        if !e.is_empty() {
            eprintln!("{e}");
        }
        eprintln!("{USAGE}");
        "invalid arguments"
    })?;

    if let Some((old_path, new_path)) = &cfg.diff {
        let (old, new) = (read_tsv(old_path)?, read_tsv(new_path)?);
        let profiles: BTreeSet<&str> = new.iter().map(|r| r.profile.as_str()).collect();
        for p in profiles {
            let old_sel: Vec<Row> = select(&old, p).into_iter().cloned().collect();
            let new_sel: Vec<Row> = new.iter().filter(|r| r.profile == p).cloned().collect();
            compare(
                &old_sel,
                &new_sel,
                &format!("{p}: {} vs {}", new_path.display(), old_path.display()),
            );
        }
        return Ok(());
    }

    if cfg.butteraugli && !cfg!(feature = "butteraugli") {
        return Err("built without the butteraugli feature".into());
    }
    if let Some(dir) = &cfg.out_dir {
        std::fs::create_dir_all(dir)?;
    }
    let pool = match cfg.threads {
        Some(n) => Some(rayon::ThreadPoolBuilder::new().num_threads(n).build()?),
        None => None,
    };
    if pool.is_some() && !cfg!(feature = "threads") {
        eprintln!("note: the library is built without threads, --threads has no effect on it");
    }

    let settings = settings_comment(&cfg);
    out!("{settings}");
    out!("{}", Row::header());
    let mut rows = Vec::new();
    let started = Instant::now();
    for path in list_images(&cfg.inputs)? {
        match evaluate(&cfg, pool.as_ref(), &path) {
            Ok(r) => {
                for row in r {
                    out!("{}", row.tsv());
                    rows.push(row);
                }
            }
            Err(e) => eprintln!("{}: ERROR {e}", path.display()),
        }
    }
    let names: Vec<&str> = cfg.profiles.iter().map(|&p| profile_name(p)).collect();
    for name in &names {
        let sel: Vec<&Row> = rows.iter().filter(|r| r.profile == *name).collect();
        out!("{}", mean_row(&sel, name).tsv());
    }
    eprintln!(
        "evaluated {} rows in {:.1} s",
        rows.len(),
        started.elapsed().as_secs_f64()
    );

    if let Some(save) = &cfg.save {
        let mut f = File::create(save)?;
        writeln!(f, "{settings}")?;
        writeln!(f, "{}", Row::header())?;
        for row in &rows {
            writeln!(f, "{}", row.tsv())?;
        }
    }

    if names.len() == 2 {
        let a: Vec<Row> = rows.iter().filter(|r| r.profile == names[0]).cloned().collect();
        let b: Vec<Row> = rows.iter().filter(|r| r.profile == names[1]).cloned().collect();
        compare(&a, &b, &format!("{} vs {} (same run)", names[1], names[0]));
    }
    if let Some(path) = &cfg.compare {
        let old = read_tsv(path)?;
        for name in &names {
            let old_sel: Vec<Row> = select(&old, name).into_iter().cloned().collect();
            let new_sel: Vec<Row> = rows.iter().filter(|r| r.profile == *name).cloned().collect();
            compare(&old_sel, &new_sel, &format!("{name}: this run vs {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_roundtrip_and_tolerant_parse() {
        let row = Row {
            image: "photo-x".into(),
            profile: "compat".into(),
            values: std::array::from_fn(|i| i as f64 + 0.123_456),
            hash: "00ff".into(),
        };
        let text = format!("# comment\n{}\n{}\n", Row::header(), row.tsv());
        let back = parse_tsv(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].image, "photo-x");
        assert_eq!(back[0].hash, "00ff");
        assert_eq!(back[0].get("pixels"), 0.0);
        assert_eq!(back[0].get("ssimu2"), 3.123);
        // reordered and missing columns
        let back = parse_tsv("image\tpsnr\tprofile\nfoo\t41.5\timproved\n").unwrap();
        assert_eq!(back[0].get("psnr"), 41.5);
        assert_eq!(back[0].profile, "improved");
        assert!(back[0].get("ssimu2").is_nan());
        assert!(parse_tsv("foo\tbar\n").is_err());
    }

    #[test]
    fn select_by_profile() {
        let mk = |p: &str| Row {
            image: "a".into(),
            profile: p.into(),
            values: [0.0; COLUMNS.len()],
            hash: String::new(),
        };
        let single = vec![mk("compat")];
        assert_eq!(select(&single, "improved").len(), 1);
        let mixed = vec![mk("compat"), mk("improved")];
        assert_eq!(select(&mixed, "improved")[0].profile, "improved");
        assert!(select(&mixed, "other").is_empty());
    }

    #[test]
    fn deltas() {
        let ms = &COLUMNS[column("ms")];
        assert_eq!(fmt_delta(ms, 100.0, 90.0), "-10.00%");
        let s = &COLUMNS[column("ssimu2")];
        assert_eq!(fmt_delta(s, 80.0, 80.5), "+0.500");
        assert_eq!(class_of("sprite-goblin-4mp"), "sprite");
    }
}
