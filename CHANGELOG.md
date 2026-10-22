# Changelog

All notable changes to libimgquant are listed here. Versions follow
[Semantic Versioning](https://semver.org/). Version 3.0.0 is the first release; it is
compared with libimagequant 2.4.1, the BSD-licensed C library it is based on.

<!-- TODO(release): replace "unreleased" with the release date. -->

## 3.0.0 (unreleased)

### Overview

- **Pure Rust:** a rewrite of libimagequant 2.4.1 (tag `2.4.1`, commit
  `976e021cda07c14cd36630f611a5a6eb2cafc28e`) in Rust, `#![forbid(unsafe_code)]`, with no C
  code, no build script and no FFI. Dependencies: `rgb`, and `rayon` for the optional
  `threads` feature (on by default). MSRV 1.85.
- **License:** BSD-2-Clause, plus Jef Poskanzer's original permissive notice. No code from
  libimagequant 2.5 or later (GPL-3) was used.
- **Two profiles:** `Profile::Improved` (the default) uses the new algorithms below.
  `Profile::Compat` reproduces 2.4.1 step by step, including its mixed `f32`/`f64`
  arithmetic, apart from the bug fixes below; golden-hash tests freeze its output.

### New algorithms (`Profile::Improved`)

- **Color difference:** an OKLab-shaped opponent matrix (lightness and two chroma axes) on
  2.4.1's internal gamma, keeping its premultiplied alpha and black/white blending.
- **Median cut:** splits along the principal axis at the error-optimal point, and splits
  the box with the largest error first.
- **Refinement:** weighted Hartigan moves after the k-means (Lloyd) iterations.
- **Nearest color:** an exact search with a switching triangle-inequality search.
- **Dithering:** serpentine Floyd-Steinberg in parallel 64-row strips with warm-up rows;
  seamless and deterministic.
- **Palette order:** colors that touch in the image get adjacent indices (speeds 1-7), which
  makes filtered PNGs smaller. Translucent entries still come first.
- **Speed presets:** fewer repeated median cuts and k-means iterations.

On the project's 22-image test corpus at speed 3 without dithering, the default profile
takes 1.39 s against 4.73 s for the 2.4.1 port, scores better on SSIMULACRA2, butteraugli
and CIEDE2000, and gives 6% smaller PNGs (README, "Results").

### Faster in both profiles (output unchanged)

- The 2.4.1 nearest-color search is vectorized and prunes with the triangle inequality.
- With the `threads` feature, the histogram, contrast maps, pixel conversion, k-means
  iterations, remapping and dither map run in parallel. Parallel sums use fixed work
  chunks, so output does not depend on the number of threads.

### Rust API

- `Attributes`, `Image` and `QuantizationResult` replace `liq_attr`, `liq_image` and
  `liq_result`; errors are `Result<_, Error>`.
- Images from owned or borrowed pixels (with or without a stride), separate rows, or a row
  callback.
- Remapping into a new buffer (`remapped`), a contiguous buffer (`remap_into`) or separate
  rows (`remap_rows`); one result can remap several images.
- Log callbacks are closures.

### C API (`libimgquant-capi`, new crate)

- `libimgquant.h` with every function, type and constant of the libimagequant 2.4.1 header.
  Programs written for 2.4.x need only a new include and `-limgquant`.
- Builds a shared and a static library; `cargo cinstall` (cargo-c) installs the header and
  a `libimgquant.pc` pkg-config file.
- Additions: `liq_set_profile`/`liq_get_profile` with `LIQ_HAS_PROFILES`, and a declared
  `liq_version()`. `LIQ_VERSION` is 30000.
- `liq_get_palette` returns one stable pointer per result, valid until
  `liq_result_destroy`; in 2.4.1 an old pointer could dangle after remapping.
- `liq_image_create_rgba` returns NULL when the image can't be created; 2.4.1 wrote through
  a NULL pointer.

### Command-line tool (`imgquant`, new crate)

- A pngquant-style command-line tool built on the library: `cargo install imgquant`.
- pngquant's options: number of colors, `--quality`, `--speed`, `--nofs`/`--floyd`,
  `--posterize`, `--ext`, `-o`, `--force`, `--skip-if-larger`, `--strip`, `-v`/`-q`;
  plus `--profile` and `--threads`.
- pngquant's exit codes: 98 when the minimum quality can't be reached, 99 when the result
  would be larger (`--skip-if-larger`), 15 when the output exists without `--force`.
- Reads every PNG color type and bit depth, keeps the input's gamma, writes the smallest
  bit depth and the smaller of an unfiltered and a filtered encoding, and writes files
  atomically. Reads standard input and writes standard output with `-`.

### Bug fixes (both profiles)

- **Edge map:** high-contrast pixels no longer get garbage values (2.4.1 converted negative
  floats to bytes with undefined behavior).
- **Large images:** images over about 4 megapixels get correct contrast maps (2.4.1 reused
  one row buffer for three rows, so it saw no vertical edges and shifted the map by a row).
- **Median cut:** deep recursion, which could overflow the stack on large images with many
  near-identical colors, is now iterative.
- **Out-of-bounds reads** in rare edge cases are replaced with safe behavior.
- **Settings:** `set_dithering_level` validates the new value instead of the previous one,
  and `set_output_gamma` no longer returns a palette rounded with the previous gamma.

### Deterministic output

- Dithering noise comes from the crate's own seeded generator (the lagged-Fibonacci
  algorithm of GNU `random()`) instead of the global `rand()`.
- Sorting uses a fixed tie order instead of the platform's `qsort` order, and histogram
  hashing is little-endian on every platform.
- So output is identical on every platform. The dither map threshold is speed 5 in every
  build; 2.4.1 raised it to 7 in OpenMP builds.
