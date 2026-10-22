# libimgquant 3

[![crates.io](https://img.shields.io/crates/v/libimgquant.svg)](https://crates.io/crates/libimgquant)
[![docs.rs](https://img.shields.io/docsrs/libimgquant)](https://docs.rs/libimgquant)
[![License: BSD-2-Clause](https://img.shields.io/crates/l/libimgquant.svg)](#license)
[![MSRV 1.85](https://img.shields.io/badge/MSRV-1.85-blue.svg)](#usage)
[![CI](https://github.com/kiwina/libimgquant/actions/workflows/ci.yml/badge.svg)](https://github.com/kiwina/libimgquant/actions/workflows/ci.yml)

Converts 24/32-bit RGBA images to 8-bit palette images with alpha, for lossy PNG
compression and high-quality GIFs. Pure Rust, BSD-licensed.

libimgquant 3 starts from a faithful Rust rewrite of [libimagequant](https://pngquant.org/lib/)
**2.4.1**, the last BSD-licensed release of the library behind pngquant, and improves it
with published color quantization research.

- **BSD-2-Clause.** The 2.4.1 port was made only from the 2.4.1 sources (tag `2.4.1`, commit
  `976e021cda07c14cd36630f611a5a6eb2cafc28e`). The improvements come from papers and the
  project's own work. No code from newer libimagequant releases (GPL-3) was used. See
  [COPYRIGHT](https://docs.rs/crate/libimgquant/latest/source/COPYRIGHT).
- **100% Rust**, `#![forbid(unsafe_code)]`. No C, no build script, no FFI. A separate crate
  adds a [C API](#c-api) compatible with libimagequant 2.4.x.
- **Dependencies:** [`rgb`](https://crates.io/crates/rgb) for the pixel type, and optionally
  [`rayon`](https://crates.io/crates/rayon) (feature `threads`, on by default) and
  [`png`](https://crates.io/crates/png) (feature `png`).

## Usage

```bash
cargo add libimgquant
```

The minimum supported Rust version is 1.85. API documentation is on
[docs.rs](https://docs.rs/libimgquant); changes are listed in
[CHANGELOG.md](https://docs.rs/crate/libimgquant/latest/source/CHANGELOG.md).

```rust
let (width, height) = (640, 480);
let pixels: Vec<libimgquant::RGBA> = load_rgba_somehow();

let mut attr = libimgquant::new();
attr.set_speed(3)?;              // 1 (best) ..= 10 (fastest)
attr.set_quality(65, 90)?;       // like JPEG quality; fail below 65, aim for 90
attr.set_max_colors(256)?;

let mut image = attr.new_image(pixels, width, height, 0.0)?; // gamma 0 = sRGB
let mut result = attr.quantize(&mut image)?;
result.set_dithering_level(1.0)?; // the default is 0 (no dithering), as in 2.4.1

let (palette, indices) = result.remapped(&mut image)?;
// palette: Vec<RGBA>, translucent entries first; indices: one u8 per pixel
```

Images can be created from:

- owned pixels: `new_image`, `new_image_stride`;
- borrowed pixels: `new_image_borrowed`, `new_image_stride_borrowed`;
- separate rows: `new_image_rows`;
- a row callback: `new_image_fn`.

Remap with `remapped`, `remap_into` (a contiguous buffer) or `remap_rows`. One
`QuantizationResult` can remap several images with the same palette.

### One palette for many images

For animated GIFs, sprite sheets or tile sets, collect the colors of all images in a
`Histogram` and quantize that. Each image is weighted as `quantize` would weight it, so
colors that appear in only one frame still get palette entries, and every image is remapped
to exactly the same palette:

```rust
let mut histogram = libimgquant::Histogram::new(&attr);
for frame in &mut frames {
    histogram.add_image(&attr, frame)?; // all with the same gamma
}
let mut result = histogram.quantize(&attr)?;
result.set_dithering_level(1.0)?;
let palette = result.palette_vec(); // e.g. the GIF's global color table
for frame in &mut frames {
    let (_, indices) = result.remapped(frame)?;
    // ...
}
```

`add_colors` adds colors with pixel counts from elsewhere, and `add_fixed_color` reserves
palette entries, for example the one transparent color of a GIF.
`examples/animated_gif.rs` turns PNG frames into an animated GIF this way:

```bash
cargo run --release --features png --example animated_gif -- out.gif frame*.png --compare
```

GIF has a single transparent index and no partial alpha, so the example makes pixels with
alpha below 128 transparent and the rest opaque; `--compare` prints each frame's PSNR with
the shared palette, with the first frame's palette and with a palette of its own. Six unrelated
test images as frames (490x490 crops) reach 40.4 dB with the shared palette against 34.9 dB
with the first frame's (49.0 dB with a palette each, but a 58% larger GIF); for the example's
synthetic animation of similar frames, 52.7 dB against 52.5 dB (53.5 dB each).

### Writing PNGs

For the smallest files, write each image twice and keep the smaller result: once with
filter type None, and once with per-row filters (the png crate's `Filter::MinEntropy`).

- **Why two encodings:** the default profile orders the palette so that colors that touch
  in the image get close indices, which makes filtered encodings smaller.
- **Bit depth:** pack small palettes into 1, 2 or 4 bits per pixel.
- **tRNS:** stop the chunk after the last translucent entry. Translucent entries always
  come first.
- **Avoid adaptive filtering on its own:** the png crate's default filter makes palette
  images 15–20% larger.

With the `png` feature, `libimgquant::png_io` does all of this: `decode` reads any PNG as
RGBA with its gamma, `encode_indexed` writes the palette image, and `compress_png` converts a
PNG file in one call, like pngquant. Without the feature, the
[crate documentation](https://docs.rs/libimgquant/latest/libimgquant/#writing-pngs) has an
example with the `png` crate, and `examples/pngquant.rs` in the repository is a minimal
pngquant built on it:

```bash
cargo run --release --example pngquant -- input.png output.png --quality 65-90
```

### Features

- `threads` (default): runs quantization and remapping in parallel with rayon. Output is the
  same for any number of threads. Use `default-features = false` for a single-threaded build
  without the rayon dependency; its output can differ from the threaded build in the last
  bits (see [Threads](#threads)).
- `png`: the `png_io` module (PNG decoding and size-conscious palette PNG encoding with the
  [png](https://crates.io/crates/png) crate). The `imgquant` command-line tool uses it.

## Command-line tool

[`imgquant`](https://crates.io/crates/imgquant) is a pngquant-style command-line tool built on
this crate, with pngquant's options and exit codes:

```bash
cargo install imgquant
imgquant --quality 65-90 photo.png        # writes photo-fs8.png
imgquant 64 --nofs --ext .png --force *.png  # 64 colors, no dithering, in place
```

On the 22-image test set, the default settings shrink the PNGs to 30% of their size in
3.4 s (14 threads). See [cli/README.md](cli/README.md) for all options.

## JavaScript and WebAssembly

The npm package [`libimgquant`](https://www.npmjs.com/package/libimgquant) (directory `wasm/`)
is this library compiled to WebAssembly, for browsers and Node.js 18 or later: one ES module
with TypeScript types and no dependencies. The `.wasm`, PNG codec included, is 475 KB
(185 KB gzipped).

```bash
npm install libimgquant
```

```js
import { compressPng, quantize, quantizeFrames } from "libimgquant";

const png = compressPng(inputPng, { quality: [65, 90] }); // like pngquant
const { palette, indices } = quantize(imageData.data, width, height, { maxColors: 64 });
const { palette: shared, frames } = quantizeFrames(rgbaFrames, width, height); // one palette for all frames
```

Options are `maxColors`, `speed`, `quality` (`{min, max}` or `[min, max]`), `dithering` (0-1,
default 1) and `profile`; errors are thrown as `Error`. The WebAssembly build is
single-threaded. Build it with `wasm/build.sh`; see [wasm/npm/README.md](wasm/npm/README.md).

## Python

The [`libimgquant`](https://pypi.org/project/libimgquant/) package (directory `python/`, built
with PyO3 and maturin) has one wheel per platform for CPython 3.9 and newer (stable ABI) and no
run-time dependencies:

```bash
pip install libimgquant
```

```python
import libimgquant

small = libimgquant.compress_png(open("photo.png", "rb").read(), quality=(65, 90))

result = libimgquant.quantize(rgba, width, height, max_colors=64)  # bytes or a uint8 numpy array
result.palette, result.indices, result.quality
```

`quantize_frames` computes one palette for several images, and `quantize_image` takes and
returns Pillow images. Every function releases the GIL while it works. See
[python/README.md](python/README.md).

## Profiles

| `Profile` | What it is |
| --- | --- |
| `Improved` (default) | The version 3 algorithms, described below. |
| `Compat` | libimagequant 2.4.1 reproduced step by step, including its mixed `f32`/`f64` arithmetic, apart from the bug fixes listed at the end of this file. Golden-hash tests freeze its output. |

Select one with `Attributes::set_profile`. An image can be quantized or remapped with
either profile.

## What is new in version 3

| Area | Version 3 (`Improved`) | Basis |
| --- | --- | --- |
| Color difference | 2.4.1's internal gamma with an OKLab-shaped opponent matrix (lightness plus two chroma axes), keeping 2.4.1's premultiplied alpha and black/white blending | Ottosson, *A perceptual color space for image processing* (2020) |
| Median cut | Splits along the principal axis at the error-optimal point; the box with the largest error is split first | Wan, Wong & Prusinkiewicz (1988); Orchard & Bouman (1991); Wu (1991) |
| Refinement | Weighted Hartigan moves after k-means (Lloyd) | Hartigan & Wong (1979); Telgarsky & Vattani (2010) |
| Nearest color | Exact search: the color difference is a squared Euclidean distance, so a switching triangle-inequality search finds the true nearest color | Orchard (1991); Phillips (2002) |
| Dithering | Serpentine Floyd-Steinberg in parallel 64-row strips with warm-up rows; seamless and deterministic | — |
| Palette order | Colors that touch in the image get adjacent indices (speeds 1–7); translucent entries first | Zeng, Li & Lei (2000); Pinho & Neves (2004, 2006) |
| Speed presets | Fewer repeated median cuts and k-means iterations, since the refinement does that work better | measured on the test corpus |

**Changes in both profiles (output unchanged):**

- the 2.4.1 nearest-color search is vectorized and prunes with the triangle inequality;
- the histogram, contrast maps, pixel conversion and dither map run in parallel;
- the 2.4.1 output stays bit-identical (checked by golden tests).

## Results

The test corpus is 22 PNG images, 18.5 megapixels in total: photos, paintings, textures,
sprites with alpha, UI screenshots, flat art, noise and an already paletted image. All
settings are at 256 colors.

**How it was measured:**

- **Metrics:** SSIMULACRA 2.1 (higher is better), butteraugli 3-norm and CIEDE2000 (lower is
  better). Images with alpha are scored over a dark and a light background, keeping the
  worse score.
- **PNG size:** the best-of encoding described above, applied to every library alike.
- **Time:** the best of 2 runs on 14 threads.

The comparison with imagequant 4.4.1 (the current GPL libimagequant) is for reference
only. It ran it as a black box through its public API in a separate benchmark program;
it is not part of this project.

| Speed, dithering | Library | Time | Peak memory | SSIMULACRA2 | butteraugli | CIEDE2000 | PNG size |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 3, none | **libimgquant 3** | **1.39 s** | **83 MB** | **88.30** | **0.708** | **0.527** | **3.54 MB** |
| 3, none | 2.4.1 port (`Compat`) | 4.73 s | 87 MB | 86.43 | 0.864 | 0.548 | 3.77 MB |
| 3, none | imagequant 4.4.1 (GPL) | 1.44 s | 88 MB | 87.38 | 0.799 | 0.535 | 3.89 MB |
| 3, full | **libimgquant 3** | **1.54 s** | **83 MB** | **88.78** | **0.665** | **0.532** | **3.64 MB** |
| 3, full | imagequant 4.4.1 (GPL) | 1.95 s | 88 MB | 88.12 | 0.735 | 0.546 | 4.14 MB |
| 1, full | **libimgquant 3** | **1.74 s** | **83 MB** | **88.89** | **0.663** | **0.530** | **3.64 MB** |
| 1, full | imagequant 4.4.1 (GPL) | 2.68 s | 88 MB | 88.36 | 0.684 | 0.542 | 4.07 MB |
| 10, full | **libimgquant 3** | **0.56 s** | **9 MB** | **87.37** | **0.894** | 0.687 | 4.15 MB |
| 10, full | imagequant 4.4.1 (GPL) | 1.01 s | 69 MB | 86.78 | 0.916 | **0.672** | **4.14 MB** |
| 10, none | **libimgquant 3** | 0.48 s | **9 MB** | **86.61** | **0.971** | 0.632 | 3.74 MB |
| 10, none | imagequant 4.4.1 (GPL) | **0.43 s** | 69 MB | 85.61 | 1.060 | **0.619** | **3.72 MB** |

**Against 2.4.1:** version 3 is faster, better on every metric, and smaller at every
setting.

**Against imagequant 4.4.1:**

- **Speeds 1–3:** version 3 is faster, better on every metric, 9–12% smaller, and uses
  less memory.
- **Speed 10:**
  - **Better:** SSIMULACRA2, butteraugli, and 7× less memory. With dithering, it is also
    44% faster.
  - **About equal:** file size.
  - **Worse:** CIEDE2000 is about 2% behind. Without dithering it is about 10% slower; v4's
    own time varied by about as much between runs.
- **Why sizes are equal at speed 10:** palette reordering is skipped at speeds 8–10. It
  would make files 7% smaller but costs about 20% of the time there.

`tools/iqeval` reproduces these measurements for this library; see its README.

## Mapping from the C API

| libimagequant 2.4.1 | libimgquant |
| --- | --- |
| `liq_attr_create`, `liq_attr_copy`, `liq_attr_destroy` | `Attributes::new()` / `libimgquant::new()`, `Clone`, `Drop` |
| `liq_set/get_max_colors`, `_speed`, `_min_opacity`, `_min_posterization` | `set_*` and getter methods on `Attributes` |
| `liq_set_quality`, `liq_get_min_quality`, `liq_get_max_quality` | `set_quality(min, target)`, `quality() -> (min, target)` |
| `liq_set_last_index_transparent` | `set_last_index_transparent(bool)` |
| `liq_set_log_callback`, `liq_set_log_flush_callback` | `set_log_callback`, `set_log_flush_callback` (closures) |
| `liq_image_create_rgba`, `_rgba_rows`, `_custom` | `new_image*`, `new_image_rows`, `new_image_fn` |
| `liq_image_add_fixed_color`, `liq_image_get_width/height` | `Image::add_fixed_color`, `width()`, `height()` |
| `liq_quantize_image` | `Attributes::quantize(&mut image) -> Result<QuantizationResult, Error>` |
| `liq_set_dithering_level`, `liq_set/get_output_gamma` | methods on `QuantizationResult` |
| `liq_get_palette` | `QuantizationResult::palette() -> &[RGBA]` |
| `liq_write_remapped_image(_rows)` | `remap_into`, `remap_rows`, `remapped` |
| `liq_get_quantization_error/quality` | `quantization_error()`, `quantization_quality()` (`None` = not measured yet) |
| `liq_error` | `Error` (only the variants that can occur) |

The Rust API doesn't need custom allocators, `liq_image_set_memory_ownership`, handle
validity checks or `liq_version`, because Rust ownership replaces them; the [C API](#c-api)
provides all of them.

## C API

The crate [`libimgquant-capi`](https://crates.io/crates/libimgquant-capi) (workspace member
`capi`) builds
`libimgquant.so`/`.dylib`/`.dll` and `libimgquant.a` with the header `libimgquant.h`. The header
has the libimagequant 2.4.x API, so C and C++ programs written for 2.4.x only need a new include
and `-limgquant`:

```bash
cargo build --release -p libimgquant-capi
```

The C API's unsafe code is confined to that crate; this crate stays `#![forbid(unsafe_code)]`.

## Differences from 2.4.1 in both profiles

### Bug fixes

- **Edge map:**
  - High-contrast pixels no longer get garbage edge-map values. The original converted
    negative floats to bytes with undefined behavior.
  - Images over about 4 megapixels get correct contrast maps. The original reused one row
    buffer for the rows above, at and below each pixel, so it saw no vertical edges and
    shifted the map by a row.
- **Settings:**
  - `set_dithering_level` validates the new value instead of the previous one.
  - `set_output_gamma` no longer returns a palette rounded with the previous gamma.
  - The 2.4.1 limit of `i32::MAX` pixels per image is kept.
- **Crashes and out-of-bounds reads:**
  - Deep recursion in median cut, which could overflow the stack on large images with many
    near-identical colors, is now iterative.
  - Several out-of-bounds reads in rare edge cases are replaced with safe behavior.

### Deterministic everywhere

The original used the global `rand()` for dithering noise and the platform's `qsort` tie
order, and its histogram hash depended on endianness. This crate instead uses:

- its own seeded generator, the same lagged-Fibonacci algorithm as GNU `random()`;
- a fixed tie order;
- little-endian hashing.

So output is identical on every platform.

### Threads

- **Dither map threshold:** the original enabled the dither map up to speed 7 only in OpenMP
  builds; here the threshold is always speed 5.
- **Thread count:** with the `threads` feature, parallel sums use fixed work chunks, so
  output does not depend on the number of threads.
- **Single-threaded build:** output can differ in the last bits from the build without
  threads (`default-features = false`), which sums in exactly the original order.

## License

BSD-2-Clause, plus the original permissive notice by Jef Poskanzer. See
[COPYRIGHT](https://docs.rs/crate/libimgquant/latest/source/COPYRIGHT).
