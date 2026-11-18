//! Command-line parsing, written by hand: the pngquant-style interface has a leading
//! number of colors, an option with an optional value (`--floyd[=N]`) and getopt-style
//! short options, which small parser crates don't cover.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use libimgquant::Profile;

pub const HELP: &str = "\
imgquant: lossy PNG compressor (24/32-bit PNG to 8-bit palette PNG with alpha)

usage:  imgquant [options] [ncolors] -- pngfile [pngfile ...]
        imgquant [options] [ncolors] - >stdout <stdin

ncolors is the maximum number of colors, 2 to 256 (default 256).

options:
  -f, --force             overwrite existing output files
  -o, --output FILE       output file name (one input file only; '-' writes to stdout)
      --ext SUFFIX        suffix of output file names (default '-fs8.png', or '-or8.png'
                          without dithering); a trailing '.png' of the input is replaced
      --skip-if-larger    only save converted files that are smaller than the input
  -Q, --quality MIN-MAX   don't save below MIN quality, use the fewest colors that reach
                          MAX (0-100, like JPEG); also 'MAX', '-MAX' and 'MIN-'
  -s, --speed N           speed 1 (slowest, best) to 10 (fastest); default 3; 11 is accepted
                          and is the same as 10
      --nofs              disable Floyd-Steinberg dithering
      --ordered           same as --nofs (there is no ordered dithering)
      --floyd[=0.0..1.0]  dithering level (default 1, full dithering)
      --posterize N       ignore N least significant bits of each channel (0-4)
      --strip             don't copy the ICC profile (the gamma is always kept)
      --transbug          put the fully transparent color last in the palette
      --profile NAME      'improved' (default) or 'compat' (libimagequant 2.4.1's algorithms)
      --threads N         number of worker threads (default: one per CPU)
  -v, --verbose           print the quantizer's progress and per-file statistics
  -q, --quiet             don't print notices about skipped files (exit codes 15, 98, 99);
                          errors are still printed
  -h, --help              print this help
  -V, --version           print the version

Without --output, each input file 'name.png' is written to 'name-fs8.png' (see --ext).
Use '--ext .png --force' to overwrite the input files. '-' as the only input file reads
standard input and writes standard output (or --output). Outputs are written atomically:
a temporary file in the same directory is renamed over the target.

The output keeps the input's gamma (gAMA chunk) and, without --strip, its RGB ICC profile
(iCCP chunk); other metadata is not copied.

exit codes:
  0   success
  1   a file could not be read, decoded or written
  2   invalid command line
  15  the output file exists and --force was not given (file not written)
  98  --skip-if-larger: the result would not be smaller (file not written)
  99  the image could not reach the minimum --quality (file not written)
With several input files, every file is processed and the first non-zero code is returned.
When the output is standard output, 98 and 99 write the input there unchanged.
";

/// What the command line asks for.
pub enum Command {
    Run(Options),
    Help,
    Version,
}

#[derive(Debug)]
pub struct Options {
    pub colors: u32,
    /// `(minimum, target)`.
    pub quality: Option<(u8, u8)>,
    pub speed: i32,
    pub dithering: f32,
    pub posterize: u8,
    pub ext: Option<OsString>,
    pub output: Option<PathBuf>,
    pub force: bool,
    pub skip_if_larger: bool,
    /// `--strip`: don't copy the ICC profile.
    pub strip: bool,
    /// `--transbug`: the transparent color goes last in the palette.
    pub transparent_last: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub profile: Profile,
    pub threads: Option<usize>,
    pub files: Vec<PathBuf>,
}

impl Options {
    /// Output suffix: `--ext`, or pngquant's default for the dithering setting.
    pub fn ext(&self) -> OsString {
        self.ext.clone().unwrap_or_else(|| {
            if self.dithering > 0.0 {
                "-fs8.png".into()
            } else {
                "-or8.png".into()
            }
        })
    }
}

/// Parses the arguments after the program name. The error is a message for the user.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut opts = Options {
        colors: 256,
        quality: None,
        speed: 3,
        dithering: 1.0,
        posterize: 0,
        ext: None,
        output: None,
        force: false,
        skip_if_larger: false,
        strip: false,
        transparent_last: false,
        verbose: false,
        quiet: false,
        profile: Profile::Improved,
        threads: None,
        files: Vec::new(),
    };
    let mut colors_seen = false;
    let mut only_files = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if only_files || text == "-" || !text.starts_with('-') {
            let is_number = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
            if !only_files && !colors_seen && opts.files.is_empty() && is_number {
                opts.colors = match text.parse() {
                    Ok(n @ 2..=256) => n,
                    _ => return Err(format!("number of colors must be 2 to 256, not '{text}'")),
                };
                colors_seen = true;
            } else {
                opts.files.push(arg.into());
            }
        } else if text == "--" {
            only_files = true;
        } else if let Some(long) = text.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            // option values come from `--name=value` or the next argument
            let mut value = |what: &str| -> Result<OsString, String> {
                match inline {
                    Some(v) => Ok(v.into()),
                    None => args.next().ok_or_else(|| format!("--{name} needs {what}")),
                }
            };
            let no_value = || match inline {
                Some(_) => Err(format!("--{name} doesn't take a value")),
                None => Ok(()),
            };
            match name {
                "help" => return Ok(Command::Help),
                "version" => return Ok(Command::Version),
                "force" => no_value().map(|()| opts.force = true)?,
                "skip-if-larger" => no_value().map(|()| opts.skip_if_larger = true)?,
                "strip" => no_value().map(|()| opts.strip = true)?,
                "transbug" => no_value().map(|()| opts.transparent_last = true)?,
                "verbose" => no_value().map(|()| opts.verbose = true)?,
                "quiet" => no_value().map(|()| opts.quiet = true)?,
                "nofs" | "ordered" => no_value().map(|()| opts.dithering = 0.0)?,
                "floyd" => opts.dithering = inline.map_or(Ok(1.0), parse_dithering)?,
                "quality" => opts.quality = Some(parse_quality(&utf8(&value("MIN-MAX")?)?)?),
                "speed" => opts.speed = parse_speed(&utf8(&value("a number")?)?)?,
                "posterize" => opts.posterize = parse_posterize(&utf8(&value("a number")?)?)?,
                "ext" => opts.ext = Some(parse_ext(value("a suffix")?)?),
                "output" => opts.output = Some(value("a file name")?.into()),
                "profile" => opts.profile = parse_profile(&utf8(&value("a profile name")?)?)?,
                "threads" => opts.threads = Some(parse_threads(&utf8(&value("a number")?)?)?),
                _ => return Err(format!("unknown option '--{name}'")),
            }
        } else {
            // a cluster of short options, like -fv or -s5 or -o out.png
            let cluster = &text[1..];
            for (i, flag) in cluster.char_indices() {
                let rest = &cluster[i + flag.len_utf8()..];
                let mut value = || -> Result<OsString, String> {
                    if rest.is_empty() {
                        args.next().ok_or_else(|| format!("-{flag} needs a value"))
                    } else {
                        Ok(rest.into())
                    }
                };
                match flag {
                    'h' => return Ok(Command::Help),
                    'V' => return Ok(Command::Version),
                    'f' => opts.force = true,
                    'v' => opts.verbose = true,
                    'q' => opts.quiet = true,
                    'o' => {
                        // a non-UTF-8 file name is kept as is when it is a separate argument
                        opts.output = Some(value()?.into());
                        break;
                    }
                    's' => {
                        opts.speed = parse_speed(&utf8(&value()?)?)?;
                        break;
                    }
                    'Q' => {
                        opts.quality = Some(parse_quality(&utf8(&value()?)?)?);
                        break;
                    }
                    _ => return Err(format!("unknown option '-{flag}'")),
                }
            }
        }
    }

    if opts.files.is_empty() {
        return Err("no input files".into());
    }
    if opts.files.len() > 1 && opts.files.iter().any(|f| f.as_os_str() == "-") {
        return Err("'-' (standard input) must be the only input file".into());
    }
    if opts.files.len() > 1 && opts.output.is_some() {
        return Err("--output can only be used with one input file".into());
    }
    Ok(Command::Run(opts))
}

fn utf8(value: &OsStr) -> Result<String, String> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("'{}' is not valid UTF-8", value.to_string_lossy()))
}

fn parse_quality(text: &str) -> Result<(u8, u8), String> {
    let number = |s: &str| -> Result<u8, String> {
        match s.parse::<u8>() {
            Ok(n) if n <= 100 => Ok(n),
            _ => Err(format!("--quality: '{s}' is not a number from 0 to 100")),
        }
    };
    let (min, max) = match text.split_once('-') {
        Some(("", max)) => (0, number(max)?),
        Some((min, "")) => (number(min)?, 100),
        Some((min, max)) => (number(min)?, number(max)?),
        None => (0, number(text)?),
    };
    if min > max {
        return Err(format!("--quality: minimum {min} is above maximum {max}"));
    }
    Ok((min, max))
}

fn parse_speed(text: &str) -> Result<i32, String> {
    match text.parse() {
        Ok(n @ 1..=11) => Ok(n),
        _ => Err(format!("--speed must be 1 to 11, not '{text}'")),
    }
}

fn parse_posterize(text: &str) -> Result<u8, String> {
    match text.parse() {
        Ok(n @ 0..=4) => Ok(n),
        _ => Err(format!("--posterize must be 0 to 4, not '{text}'")),
    }
}

fn parse_dithering(text: &str) -> Result<f32, String> {
    match text.parse::<f32>() {
        Ok(level) if (0.0..=1.0).contains(&level) => Ok(level),
        _ => Err(format!("--floyd must be 0 to 1, not '{text}'")),
    }
}

fn parse_ext(ext: OsString) -> Result<OsString, String> {
    if ext.is_empty() {
        return Err("--ext must not be empty".into());
    }
    Ok(ext)
}

fn parse_profile(text: &str) -> Result<Profile, String> {
    match text {
        "improved" => Ok(Profile::Improved),
        "compat" => Ok(Profile::Compat),
        _ => Err(format!("--profile must be 'improved' or 'compat', not '{text}'")),
    }
}

fn parse_threads(text: &str) -> Result<usize, String> {
    match text.parse() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("--threads must be a positive number, not '{text}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Options, String> {
        match parse(args.iter().map(OsString::from))? {
            Command::Run(opts) => Ok(opts),
            Command::Help => Err("help".into()),
            Command::Version => Err("version".into()),
        }
    }

    #[test]
    fn quality_forms() {
        assert_eq!(parse_quality("65-90"), Ok((65, 90)));
        assert_eq!(parse_quality("80"), Ok((0, 80)));
        assert_eq!(parse_quality("-80"), Ok((0, 80)));
        assert_eq!(parse_quality("60-"), Ok((60, 100)));
        assert!(parse_quality("90-60").is_err());
        assert!(parse_quality("101").is_err());
        assert!(parse_quality("a-b").is_err());
        assert!(parse_quality("-").is_err());
    }

    #[test]
    fn leading_number_is_colors() {
        let o = run(&["64", "a.png"]).unwrap();
        assert_eq!(o.colors, 64);
        assert_eq!(o.files, [PathBuf::from("a.png")]);
        // only the first positional argument, and not after `--`
        let o = run(&["a.png", "64"]).unwrap();
        assert_eq!(o.colors, 256);
        assert_eq!(o.files.len(), 2);
        let o = run(&["--", "64"]).unwrap();
        assert_eq!(o.files, [PathBuf::from("64")]);
        assert!(run(&["1", "a.png"]).is_err());
        assert!(run(&["257", "a.png"]).is_err());
    }

    #[test]
    fn options_and_clusters() {
        let o = run(&[
            "-fvs7",
            "-Q",
            "50-70",
            "--floyd=0.5",
            "--ext=.x.png",
            "-oout.png",
            "a.png",
        ])
        .unwrap();
        assert!(o.force && o.verbose);
        assert_eq!(o.speed, 7);
        assert_eq!(o.quality, Some((50, 70)));
        assert_eq!(o.dithering, 0.5);
        assert_eq!(o.ext(), ".x.png");
        assert_eq!(o.output, Some(PathBuf::from("out.png")));
        let o = run(&["--nofs", "--posterize", "2", "--profile", "compat", "--strip", "a.png"]).unwrap();
        assert_eq!(o.dithering, 0.0);
        assert_eq!(o.ext(), "-or8.png");
        assert_eq!(o.posterize, 2);
        assert_eq!(o.profile, Profile::Compat);
        assert!(o.strip && !o.transparent_last);
        assert_eq!(run(&["--floyd", "a.png"]).unwrap().dithering, 1.0);
        let o = run(&["--ordered", "--transbug", "-s11", "a.png"]).unwrap();
        assert_eq!(o.dithering, 0.0);
        assert_eq!(o.ext(), "-or8.png");
        assert!(o.transparent_last && !o.strip);
        assert_eq!(o.speed, 11);
    }

    #[test]
    fn errors() {
        for args in [
            &[][..],
            &["--speed", "0", "a.png"],
            &["--speed", "12", "a.png"],
            &["--transbug=1", "a.png"],
            &["--speed"],
            &["--force=1", "a.png"],
            &["--floyd=2", "a.png"],
            &["--posterize", "5", "a.png"],
            &["--bogus", "a.png"],
            &["-x", "a.png"],
            &["-", "a.png"],
            &["-o", "x.png", "a.png", "b.png"],
            &["--ext=", "a.png"],
            &["--threads", "0", "a.png"],
        ] {
            assert!(run(args).is_err(), "{args:?}");
        }
        assert_eq!(run(&["a.png", "-h"]).unwrap_err(), "help");
        assert_eq!(run(&["--version"]).unwrap_err(), "version");
    }
}
