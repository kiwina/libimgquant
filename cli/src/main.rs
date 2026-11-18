//! `imgquant`: a pngquant-style command-line tool built on libimgquant.
//!
//! See `imgquant --help` and the README for the options and exit codes.

#![forbid(unsafe_code)]

mod args;

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use args::{Command, Options};
use libimgquant::png_io::{self, DecodedPng, PngError};

/// A file could not be read, decoded or written.
const EXIT_IO: u8 = 1;
/// Invalid command line.
const EXIT_USAGE: u8 = 2;
/// The output exists and `--force` was not given.
const EXIT_NOT_OVERWRITING: u8 = 15;
/// `--skip-if-larger` and the result is not smaller than the input.
const EXIT_TOO_LARGE: u8 = 98;
/// The minimum `--quality` could not be reached (pngquant's 99, libimagequant's
/// `LIQ_QUALITY_TOO_LOW`).
const EXIT_QUALITY_TOO_LOW: u8 = 99;

/// Why one file was not converted: an exit code and a message.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn main() -> ExitCode {
    let opts = match args::parse(std::env::args_os().skip(1)) {
        Ok(Command::Run(opts)) => opts,
        Ok(Command::Help) => {
            print!("{}", args::HELP);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("imgquant {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("imgquant: {message}\nTry 'imgquant --help' for more information.");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    if let Some(threads) = opts.threads {
        #[cfg(feature = "threads")]
        if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global() {
            eprintln!("imgquant: --threads: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
        #[cfg(not(feature = "threads"))]
        if !opts.quiet {
            eprintln!("imgquant: built without the threads feature; ignoring --threads {threads}");
        }
    }

    let mut status = 0;
    for input in &opts.files {
        if let Err(failure) = convert(&opts, input) {
            if !opts.quiet || failure.code == EXIT_IO {
                eprintln!("imgquant: {}", failure.message);
            }
            if status == 0 {
                status = failure.code;
            }
        }
    }
    ExitCode::from(status)
}

/// Where a converted image goes.
enum Target {
    Stdout,
    File(PathBuf),
}

fn is_dash(path: &Path) -> bool {
    path.as_os_str() == "-"
}

/// `name.png` -> `name` + `ext`; other names get `ext` appended.
fn output_name(input: &Path, ext: &OsString) -> PathBuf {
    let stem = match input.extension() {
        Some(e) if e.eq_ignore_ascii_case("png") => input.with_extension(""),
        _ => input.to_path_buf(),
    };
    let mut name = stem.into_os_string();
    name.push(ext);
    name.into()
}

fn convert(opts: &Options, input: &Path) -> Result<(), Failure> {
    let start = Instant::now();
    let name = input.display();
    let target = match &opts.output {
        Some(out) if is_dash(out) => Target::Stdout,
        Some(out) => Target::File(out.clone()),
        None if is_dash(input) => Target::Stdout,
        None => Target::File(output_name(input, &opts.ext())),
    };
    if let Target::File(out) = &target {
        // checked before the work is done; checked again before the file is replaced
        if !opts.force && out.exists() {
            return Err(not_overwriting(out));
        }
    }

    let data = if is_dash(input) {
        let mut data = Vec::new();
        io::stdin()
            .lock()
            .read_to_end(&mut data)
            .map_err(|e| Failure::new(EXIT_IO, format!("standard input: {e}")))?;
        data
    } else {
        fs::read(input).map_err(|e| Failure::new(EXIT_IO, format!("{name}: {e}")))?
    };
    let decoded = png_io::decode(&data).map_err(|e| match e {
        PngError::Decode(e) => Failure::new(EXIT_IO, format!("{name}: not a valid PNG: {e}")),
        e => Failure::new(EXIT_IO, format!("{name}: {e}")),
    })?;
    if opts.verbose {
        let gamma = match decoded.gamma {
            Some(g) => format!("gamma {g}"),
            None => "sRGB".into(),
        };
        eprintln!(
            "{name}:\n  read {} bytes, {}x{}, {gamma}",
            data.len(),
            decoded.width,
            decoded.height
        );
    }

    let quantize_start = Instant::now();
    let (palette, indices, quality) = quantize(opts, &decoded).map_err(|e| match e {
        libimgquant::Error::QualityTooLow => reject(
            &target,
            &data,
            EXIT_QUALITY_TOO_LOW,
            format!(
                "{name}: the minimum quality {} could not be reached",
                opts.quality.map_or(0, |q| q.0)
            ),
        ),
        e => Failure::new(EXIT_IO, format!("{name}: {e}")),
    })?;
    let quantize_time = quantize_start.elapsed();

    // the gAMA chunk is always kept: it changes how the colors are displayed
    let mut metadata = decoded.metadata();
    if opts.strip {
        metadata.icc_profile = None;
    }
    let png = png_io::encode_indexed(&palette, &indices, decoded.width, decoded.height, &metadata).map_err(|e| {
        let e: &dyn std::fmt::Display = match &e {
            PngError::Encode(e) => e,
            e => e,
        };
        Failure::new(EXIT_IO, format!("{name}: {e}"))
    })?;
    if opts.verbose {
        eprintln!(
            "  {} colors, quality {} (speed {}, dithering {}), quantized in {:.1} ms",
            palette.len(),
            quality.map_or("unknown".into(), |q| q.to_string()),
            opts.speed,
            opts.dithering,
            quantize_time.as_secs_f64() * 1000.0
        );
    }
    if opts.skip_if_larger && png.len() >= data.len() {
        return Err(reject(
            &target,
            &data,
            EXIT_TOO_LARGE,
            format!(
                "{name}: the result ({} bytes) would not be smaller than the input ({} bytes)",
                png.len(),
                data.len()
            ),
        ));
    }

    let written_to = match &target {
        Target::Stdout => {
            write_stdout(&png)?;
            "standard output".to_string()
        }
        Target::File(out) => {
            write_atomic(out, &png, opts.force)?;
            out.display().to_string()
        }
    };
    if opts.verbose {
        eprintln!(
            "  wrote {} bytes to {written_to} ({:.1}% of the input), {:.1} ms in total",
            png.len(),
            100.0 * png.len() as f64 / data.len() as f64,
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

/// Quantizes and remaps the image: the palette, one index per pixel, and the quality.
type Quantized = (Vec<libimgquant::RGBA>, Vec<u8>, Option<u8>);

fn quantize(opts: &Options, decoded: &DecodedPng) -> Result<Quantized, libimgquant::Error> {
    let mut attr = libimgquant::new();
    attr.set_profile(opts.profile);
    // pngquant's fastest speed is 11, the quantizer's is 10
    attr.set_speed(opts.speed.min(10))?;
    attr.set_last_index_transparent(opts.transparent_last);
    attr.set_max_colors(opts.colors)?;
    attr.set_min_posterization(opts.posterize)?;
    if let Some((min, max)) = opts.quality {
        attr.set_quality(min, max)?;
    }
    if opts.verbose {
        attr.set_log_callback(|_, message| eprintln!("{message}"));
    }
    let mut image = attr.new_image_borrowed(&decoded.pixels, decoded.width, decoded.height, decoded.image_gamma())?;
    let mut result = attr.quantize(&mut image)?;
    result.set_dithering_level(opts.dithering)?;
    let (palette, indices) = result.remapped(&mut image)?;
    Ok((palette, indices, result.quantization_quality()))
}

/// The image is not saved (exit 98 or 99). Like pngquant, standard output gets the input
/// unchanged instead, so that a pipeline still gets a PNG.
fn reject(target: &Target, data: &[u8], code: u8, reason: String) -> Failure {
    match target {
        Target::File(_) => Failure::new(code, format!("{reason}; not saved")),
        Target::Stdout => match write_stdout(data) {
            Ok(()) => Failure::new(code, format!("{reason}; wrote the input unchanged to standard output")),
            Err(failure) => failure,
        },
    }
}

fn write_stdout(data: &[u8]) -> Result<(), Failure> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(data)
        .and_then(|()| stdout.flush())
        .map_err(|e| Failure::new(EXIT_IO, format!("standard output: {e}")))
}

fn not_overwriting(out: &Path) -> Failure {
    Failure::new(
        EXIT_NOT_OVERWRITING,
        format!("{}: already exists; not overwriting (use --force)", out.display()),
    )
}

/// Writes `data` to a temporary file next to `path`, then renames it over `path`, so that
/// `path` is never left truncated.
fn write_atomic(path: &Path, data: &[u8], force: bool) -> Result<(), Failure> {
    let io_error = |e: io::Error| Failure::new(EXIT_IO, format!("{}: {e}", path.display()));
    let file_name = path
        .file_name()
        .ok_or_else(|| io_error(io::Error::new(io::ErrorKind::InvalidInput, "not a file name")))?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut attempt = 0;
    let (temp, mut file) = loop {
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".{}-{attempt}.tmp", std::process::id()));
        let temp = dir.join(temp_name);
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            Err(e) => return Err(io_error(e)),
        }
    };
    let result = file.write_all(data).and_then(|()| file.flush());
    drop(file);
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(io_error(e));
    }
    if !force && path.exists() {
        let _ = fs::remove_file(&temp);
        return Err(not_overwriting(path));
    }
    // a replaced file keeps its permissions
    if let Ok(existing) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp, existing.permissions());
    }
    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        io_error(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_names() {
        let fs8 = OsString::from("-fs8.png");
        assert_eq!(output_name(Path::new("a.png"), &fs8), Path::new("a-fs8.png"));
        assert_eq!(output_name(Path::new("dir/b.PNG"), &fs8), Path::new("dir/b-fs8.png"));
        assert_eq!(output_name(Path::new("c.tar.png"), &fs8), Path::new("c.tar-fs8.png"));
        assert_eq!(output_name(Path::new("d.gif"), &fs8), Path::new("d.gif-fs8.png"));
        assert_eq!(output_name(Path::new("e"), &".png".into()), Path::new("e.png"));
    }
}
