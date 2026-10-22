# iqeval

Quality, file-size and speed evaluation for `libimgquant`. It quantizes every PNG
in a set through the library's public API, scores the result with several perceptual
and classic metrics, writes the PNG the way a size-conscious encoder should, and compares
runs image by image.

It is a separate workspace member with `publish = false`, so its dependencies and its
MSRV (1.89, for butteraugli) never reach the library, which keeps MSRV 1.85 and its two
dependencies. `cargo build`/`cargo test` in the repository root build only the library;
use `-p iqeval` for this tool.

## Usage

```bash
cargo run --release -p iqeval -- <dir-or-png>... [options]
cargo run --release -p iqeval -- --diff old.tsv new.tsv
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--profile compat\|improved\|both` | `compat` | `both` runs the two profiles interleaved (A B A B per repetition, so load drifts hit both alike) and prints improved vs compat |
| `--speed N` | 3 | `set_speed` |
| `--dither F` | 1.0 | `set_dithering_level` (the library default is 0) |
| `--colors N` | 256 | `set_max_colors` |
| `--quality MIN-MAX` | none | `set_quality`; images that fail the minimum are reported as errors |
| `--repeat N` | 1 | time is the minimum of N runs; every run must give the same output, or a warning is printed |
| `--threads N` | rayon default | run the quantizer in an N-thread rayon pool (`--threads 1` times it single-threaded). Metrics always use the global pool |
| `--no-butteraugli` | off | skip butteraugli (about 15% of the metric time) |
| `--out DIR` | none | also write each output as `DIR/<image>.<profile>.png` for visual checks |
| `--save run.tsv` | none | write the per-image rows as TSV |
| `--compare old.tsv` | none | after the run, compare it with a saved one |
| `--diff old.tsv new.tsv` | | compare two saved runs without quantizing anything |

Cargo features: `threads` (default) builds the library with its `threads` feature;
`--no-default-features --features butteraugli` measures the sequential library build.
`butteraugli` (default) can be left out to build without that dependency.

Typical A/B session for a change to the Improved profile:

```bash
C=path/to/corpus
cargo run --release -p iqeval -- $C --profile both --repeat 3 --dither 1 --save s3d1.tsv
cargo run --release -p iqeval -- $C --profile both --repeat 3 --dither 0 --save s3d0.tsv
```

and for a change that must keep output identical (Compat), save a run before and after and
check `identical outputs: 22/22` in `--diff`.

On the 22-image, 19-megapixel `corpus-v1` a single run takes about 40 s (most of it
SSIMULACRA2), and `--profile both --repeat 3` about 2-3 minutes on a 14-thread machine.
Peak memory is about 2.5 GB, for a 4-megapixel image with alpha.

## Metrics

Each image row has:

| Column | Better | What |
| --- | --- | --- |
| `pixels`, `colors` | | image size, palette entries used |
| `ms` | lower | minimum wall time of `new_image_borrowed` + `quantize` + `set_dithering_level` + `remapped` (no PNG decoding or encoding) |
| `ssimu2` | higher | SSIMULACRA 2.1 (`ssimulacra2` crate). 100 = identical, 90 visually lossless, 70 high quality. The primary perceptual score |
| `butter_p3`, `butter_max` | lower | butteraugli 3-norm (libjxl's `--pnorm`) and max-norm (`butteraugli` crate) |
| `de2000` | lower | mean CIEDE2000 (Sharma, Wu and Dalal 2005), CIELAB with a D65 white. The tie-breaker least biased toward XYB/OKLab-like spaces |
| `deltaE`, `deltaE_p99` | lower | mean and 99th percentile of the OKLab distance (Ottosson 2020), times 100 |
| `deltaE_blur` | lower | mean OKLab distance x100 after a Gaussian blur with sigma 1 px in linear light, a crude spatial model of the eye in the spirit of S-CIELAB (Zhang and Wandell 1997). Dithering that averages to the right color scores well here, unlike in the per-pixel metrics |
| `psnr` | higher | PSNR of composited sRGB values. A sanity check only; it ranks perceptual improvements backwards |
| `qerr` | lower | the library's own `quantization_error()` (its internal 2.4.1 metric, scaled like pngquant's `--verbose`) |
| `png_bytes`, `bpp` | lower | size of the indexed PNG (see below), and bits per pixel |
| `hash` | | FNV-1a hash of palette and indices: equal hashes mean bit-identical output |

**Alpha.** Images (original or output) with any non-opaque pixel are composited over
gray 0.1 and gray 0.9 in sRGB-encoded space, as browsers do and as ssimulacra2's reference
tool does, and each metric reports the worse of the two backgrounds. One background hides
errors that the other reveals (for example wrong colors under nearly transparent pixels).
Opaque images use one 0.5 background, which has no effect.

**Determinism.** Every floating-point sum runs over fixed chunks combined in order, so
scores do not depend on the number of threads. Values are compared after rounding to the
TSV precision, so differences below it count as ties.

**PNG size.** Indexed color, filter type None (PNG spec section 12.7 recommends it for
palette images; the png crate's default is adaptive filtering, which costs 14-21% here),
the smallest bit depth holding the palette (1, 2, 4 or 8 bits), a tRNS chunk trimmed after
the last non-opaque entry, and `Compression::High`. Note that png 0.18's
`set_compression` also resets the filter, so `set_filter` must come after it.

## Comparison output

`--compare`, `--diff` and `--profile both` print:

1. Per-image changes: absolute for `ssimu2` and `psnr`, relative for the rest, `=` when
   equal, and whether the output is bit-identical.
2. `identical outputs: k/n`.
3. Per metric: means before and after, wins/losses/ties (a win is an image where the new
   run is better), the exact two-sided Wilcoxon signed-rank p-value over the paired
   differences (zeros dropped, ties given average ranks), and for relative metrics the
   geometric mean of the per-image ratios, which is the fair summary for bytes and time.
4. Per image class (the file name up to the first `-`): mean changes and geometric-mean
   byte and time ratios.

Rows are paired by image name. When the old file holds several profiles, each new profile
is compared with the same profile; when it holds one, every new profile is compared with it.

Timing on a shared machine is noisy: compare `ms` only with `--repeat 3` or more, preferably
in one `--profile both` run, and treat a few percent as noise (a Compat vs Improved run with
identical outputs shows the noise floor directly).

## Licenses of the dependencies

All permissive, confirmed from each crate's `Cargo.toml` and license files in the cargo
registry: `ssimulacra2` 0.5.1 (BSD-2-Clause, LICENSE file), its `yuvxyb` 0.4 and
`yuvxyb-math` (MIT), `butteraugli` 0.9.3 (BSD-3-Clause in Cargo.toml and README; the crate
ships no LICENSE file), `png` 0.18 (MIT OR Apache-2.0), `rayon` (MIT OR Apache-2.0),
`miniz_oxide` (tests only; MIT OR Zlib OR Apache-2.0). The other crates they pull in are
`v_frame` (BSD-2-Clause), `imgref` (CC0-1.0 OR Apache-2.0), `unicode-ident`
((MIT OR Apache-2.0) AND Unicode-3.0), `bytemuck` (Zlib OR Apache-2.0 OR MIT), and MIT or
MIT OR Apache-2.0 for the rest (`aligned-vec`, `archmage`, `magetypes`, `av-data`, `equator`,
`num-*`, `thiserror`, `syn`, ...). None is GPL, LGPL, AGPL or MPL. dssim is deliberately
not used (AGPL-3.0).
