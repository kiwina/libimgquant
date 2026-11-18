# imgquant

A command-line lossy PNG compressor with a pngquant-style interface. It converts 24/32-bit
PNG images to 8-bit palette PNGs with alpha, using
[libimgquant](https://crates.io/crates/libimgquant), the pure-Rust, BSD-licensed color
quantizer. 100% Rust, no C dependencies. On libimgquant's private 22-image test set (photos,
sprites with alpha, UI screenshots, art; see
[BENCHMARKS.md](https://docs.rs/crate/libimgquant/latest/source/BENCHMARKS.md)), the output
is 30% of the input size in total.

## Install

```bash
cargo install imgquant
```

Prebuilt binaries for Linux (static, x86_64 and aarch64), Windows (x64) and macOS (arm64,
x86_64) are attached to each [GitHub release](https://github.com/kiwina/libimgquant/releases);
`cargo binstall imgquant` downloads the right one.

From a checkout of the repository: `cargo install --path cli`. The build is
multi-threaded by default; `--no-default-features` builds a single-threaded binary.

## Usage

```text
imgquant [options] [ncolors] -- pngfile [pngfile ...]
imgquant [options] [ncolors] - >stdout <stdin
```

`ncolors` is the maximum number of colors, 2 to 256 (default 256). Each `name.png` is
written to `name-fs8.png` (or `name-or8.png` without dithering), next to the input.

```bash
# convert every PNG in the directory to *-fs8.png
imgquant *.png

# at most 64 colors, written to a chosen file
imgquant 64 --output small.png photo.png

# replace the files in place, but only when they get smaller and keep at least quality 65
imgquant --quality 65-90 --skip-if-larger --ext .png --force *.png

# in a pipeline
curl -s https://example.com/image.png | imgquant - > image-small.png

# fastest, without dithering
imgquant --speed 10 --nofs sprite.png
```

On Windows, cmd and PowerShell don't expand `*.png`. In PowerShell, pass the files with
`imgquant (Get-ChildItem *.png).FullName`.

## Options

| Option | Meaning |
| --- | --- |
| `ncolors` | Maximum number of colors, 2–256 (default 256). Only as the first non-option argument. |
| `-f`, `--force` | Overwrite existing output files. |
| `-o`, `--output FILE` | Output file name; one input file only. `-` writes to standard output. |
| `--ext SUFFIX` | Suffix of output file names, replacing a trailing `.png` of the input. Default `-fs8.png`, or `-or8.png` when dithering is off. |
| `--skip-if-larger` | Only save converted files that are smaller than the input. |
| `-Q`, `--quality MIN-MAX` | Quality 0–100, like JPEG. The fewest colors that reach `MAX` are used; if even `ncolors` colors can't reach `MIN`, the file is not saved (exit code 99). Also `MAX` or `-MAX` (minimum 0) and `MIN-` (maximum 100). |
| `-s`, `--speed N` | 1 (slowest, best) to 10 (fastest). Default 3. 11, pngquant's fastest, is accepted and is the same as 10. |
| `--nofs`, `--ordered` | No dithering. |
| `--floyd[=LEVEL]` | Floyd-Steinberg dithering level 0.0–1.0; default 1 (full). The level must be given with `=`. |
| `--posterize N` | Ignore the N least significant bits of each channel (0–4), for displays with fewer bits per channel. |
| `--strip` | Don't copy the ICC profile. The gamma is always kept. |
| `--transbug` | Put the fully transparent color last in the palette, for software that expects it there. The tRNS chunk then covers the whole palette, and with `--profile improved` such images are not reordered for compression, so they can be up to about 10% larger. Images without a fully transparent color are unaffected. |
| `--profile NAME` | `improved` (default): libimgquant 3's algorithms. `compat`: libimagequant 2.4.1's algorithms. |
| `--threads N` | Number of worker threads (default: one per CPU). |
| `-v`, `--verbose` | Print the quantizer's progress and per-file statistics (size, colors, quality, time) to standard error. |
| `-q`, `--quiet` | Don't print notices about skipped files (exit codes 15, 98, 99). Errors are still printed. |
| `-h`, `--help` | Print help. |
| `-V`, `--version` | Print the version. |

Short options can be combined (`-fv`), and option values can be given as `--speed 5`,
`--speed=5`, `-s5` or `-s 5`. Everything after `--` is a file name.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | A file could not be read, decoded or written. |
| 2 | Invalid command line. |
| 15 | The output file already exists and `--force` was not given. The file is not written. |
| 98 | `--skip-if-larger`, and the result would not be smaller than the input. The file is not written. |
| 99 | The image could not reach the minimum `--quality`. The file is not written. |

With several input files, every file is processed in order, and the exit code is that of the
first file that failed. When the output is standard output (`-o -`, or `-` as the input
without `-o`), 98 and 99 write the input there unchanged, so a pipeline still gets a PNG.

## Output files

- **Size:** each image is encoded twice, with PNG filter None and with per-row filters chosen
  by minimum entropy, and the smaller file is kept. The bit depth is the smallest that holds
  the palette (1, 2, 4 or 8 bits), and the tRNS chunk stops after the last translucent entry.
- **Atomic writes:** the image is written to a temporary file in the output directory, which
  is then renamed over the target, so a failure never leaves a truncated or half-written
  file.
- **Input formats:** any PNG: gray, gray with alpha, RGB, RGBA or palette, 1–16 bits per
  sample, with or without tRNS transparency, interlaced or not. Of an animated PNG, the
  default image is read.
- **Gamma:** a gAMA chunk is used as the image's gamma during quantization and written to
  the output unchanged. Images with an sRGB chunk, or with no color space chunk, are treated
  as sRGB and written without one.
- **ICC profile:** an RGB ICC profile (iCCP chunk) is copied to the output unless `--strip`
  is given. A gray profile is not copied, because the output is a color (palette) image. The
  profile's name is not kept (the png crate writes `_`).

## Differences from pngquant

- **Metadata:** only the gamma (gAMA) and an RGB ICC profile (iCCP) are carried over, and
  `--strip` drops only the ICC profile. Text, EXIF, sRGB, cHRM, pHYs and other chunks are
  not copied.
- **Quantizer:** the default `improved` profile gives better quality and smaller files than
  libimagequant 2.4.1; `--profile compat` reproduces 2.4.1's palettes and remapping. The
  output is the same on every platform and for every `--threads` value.
- **`--floyd`:** the level is only accepted as `--floyd=LEVEL`, so that a following number is
  not taken as its value.
- **`--speed 11`:** the same as 10. pngquant's speed 11 also turns dithering off.
- **Not implemented:** `--map`, `--iebug` and other options not listed above are rejected
  with exit code 2.
- **`--skip-if-larger`:** compares the output with the input file size only.
- **Error exit codes:** imgquant uses 1 for any read, decode or write error and 2 for an
  invalid command line. pngquant's codes for these cases differ (2 is a read error there);
  0, 15, 98 and 99 mean the same in both.

## License

BSD-2-Clause, plus the original permissive notice by Jef Poskanzer. See
[LICENSE](https://docs.rs/crate/imgquant/latest/source/LICENSE) and
[NOTICE](https://docs.rs/crate/imgquant/latest/source/NOTICE).
