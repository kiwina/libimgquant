# Benchmarks

The measurements behind the comparison in the [README](README.md#comparison): libimgquant 3
(`Profile::Improved`) against its own port of libimagequant 2.4.1 (`Profile::Compat`) and,
for reference only, against imagequant 4.4.1.

## Test set

22 PNG images, 18.5 megapixels in total: photos, paintings, textures, sprites with alpha, UI
screenshots, flat art, noise and an already paletted image. The images are not part of the
repository; any directory of PNG files can be measured the same way. All settings are at
256 colors.

## How it was measured

- **Metrics:** SSIMULACRA 2.1 (higher is better), butteraugli 3-norm and CIEDE2000 (lower is
  better). Images with alpha are scored over a dark and a light background, keeping the
  worse score.
- **PNG size:** the smaller of an unfiltered and a filtered encoding, with the smallest bit
  depth and a trimmed tRNS chunk (as described under
  [PNG files](README.md#png-files)), applied to every library alike.
- **Time:** the best of 2 runs on 14 threads.
- **Peak memory:** the largest heap growth during one image's `new_image_borrowed`,
  `quantize` and `remapped` calls (or the same calls of imagequant), the maximum over the 22
  images, counted with a tracking allocator in the comparison program. The caller's input
  pixels are borrowed and not counted; the returned indices are.

imagequant 4.4.1 (the current GPL libimagequant) is shown for reference only. It was run as
a black box through its public API in a separate benchmark program; it is not part of this
project, and none of its code is used here.

## Results

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

**Against the 2.4.1 port** (measured at speed 3 without dithering): version 3 is 3.4 times
as fast, better on all three metrics and 6% smaller.

**Against imagequant 4.4.1:**

- **Speeds 1 and 3:** version 3 is faster, better on all three metrics, 9–12% smaller, and
  uses less memory.
- **Speed 10:**
  - **Better:** SSIMULACRA2, butteraugli, and 7× less memory. With dithering, it is also
    1.8 times as fast.
  - **About equal:** file size.
  - **Worse:** CIEDE2000 is about 2% behind. Without dithering it is about 10% slower; v4's
    own time varied by about as much between runs.
- **Why sizes are equal at speed 10:** palette reordering is skipped at speeds 8–10. It
  would make files 7% smaller but costs about 20% of the time there.

## One palette for many images

`examples/animated_gif.rs` builds one palette for all frames with a `Histogram` and writes
an animated GIF. GIF has a single transparent index and no partial alpha, so the example
makes pixels with alpha below 128 transparent and the rest opaque. `--compare` prints each
frame's PSNR with the shared palette, with the first frame's palette and with a palette of
its own:

- **Six unrelated test images as frames** (490x490 crops): 40.4 dB with the shared palette
  against 34.9 dB with the first frame's (49.0 dB with a palette each, but a 58% larger GIF).
- **The example's synthetic animation of similar frames:** 52.7 dB against 52.5 dB (53.5 dB
  each).

## Reproducing

[`tools/iqeval`](tools/iqeval/README.md) reproduces the time, quality and PNG-size columns
for this library (peak memory comes from the comparison program). It is an unpublished workspace member
with its own dependencies and MSRV (1.89). It reads PNG files, so the reference images are
exact, and it times only `new_image_borrowed`, `quantize`, `set_dithering_level` and
`remapped`:

```bash
# speed 3 (the default); add --speed 1 or --speed 10 for the other rows
cargo run --release -p iqeval -- path/to/pngs --profile both --repeat 2 --dither 1 --save s3d1.tsv
cargo run --release -p iqeval -- path/to/pngs --profile both --repeat 2 --dither 0 --save s3d0.tsv
cargo run --release -p iqeval -- --diff old.tsv new.tsv
```

`--profile both` runs `Improved` and `Compat` interleaved and prints the change per image and
per metric. See [tools/iqeval/README.md](tools/iqeval/README.md) for all options and metrics.
