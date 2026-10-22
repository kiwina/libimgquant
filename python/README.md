# libimgquant for Python

Converts RGBA images to 8-bit palette images with alpha, for small PNGs and GIFs, like
pngquant. Pure Rust underneath, BSD-licensed, with no runtime dependencies.

libimgquant 3 is a Rust rewrite of libimagequant **2.4.1**, the last BSD-licensed release
of the library behind pngquant, improved with published color quantization research: a
variance-based median cut, Hartigan refinement after k-means, an exact nearest-color search,
an OKLab-shaped color difference and a palette order that makes PNGs smaller.
`profile="compat"` reproduces 2.4.1's algorithms instead, apart from a few bug fixes. The
[Rust crate's README](https://crates.io/crates/libimgquant) describes the algorithms and
compares them with libimagequant 2.4.1 and 4.4.1.

- **BSD-2-Clause**, no GPL code: usable in closed-source and commercial software.
- **One wheel per platform** for CPython 3.9 and newer (stable ABI, `abi3`).
- **Multi-threaded**, and every function releases the GIL while it works.
- **Typed**: ships type hints (`py.typed`).

## Install

```bash
pip install libimgquant
pip install "libimgquant[pillow]"   # also installs Pillow, for quantize_image()
```

From a checkout of the repository, with a Rust toolchain installed: `pip install ./python`
(maturin builds the wheel).

## Compress a PNG file

```python
import libimgquant

with open("photo.png", "rb") as f:
    data = f.read()

small = libimgquant.compress_png(data, quality=(65, 90))
with open("photo-fs8.png", "wb") as f:
    f.write(small)
```

`compress_png` reads any PNG and writes a palette PNG with the same dimensions and gamma, the
smallest bit depth, a tRNS chunk only as long as needed, and the smaller of a filtered and
an unfiltered encoding.

## Quantize pixels

`quantize` takes `width * height` RGBA pixels as any bytes-like object (`width * height * 4`
bytes, row by row) and returns the palette and one palette index per pixel:

```python
import libimgquant

width, height = 64, 48
pixels = bytes(
    v
    for y in range(height)
    for x in range(width)
    for v in (x * 4, y * 5, 128, 100 if y < 8 else 255)
)

result = libimgquant.quantize(pixels, width, height, max_colors=64)
result.palette   # [(r, g, b, a), ...], at most 64 entries, translucent entries first
result.indices   # bytes, one palette index per pixel
result.quality   # quality reached, 0-100
```

### numpy

A `uint8` array of shape `(height, width, 4)` works as it is:

```python
import numpy as np
import libimgquant

rgba = np.zeros((480, 640, 4), dtype=np.uint8)  # your image
result = libimgquant.quantize(rgba, 640, 480)

palette = np.array(result.palette, dtype=np.uint8)              # (colors, 4)
indices = np.frombuffer(result.indices, np.uint8).reshape(480, 640)
restored = palette[indices]                                      # (480, 640, 4)
```

### Pillow

`quantize_image` takes a Pillow image in any mode, so any format Pillow opens (JPEG, WebP,
GIF, ...) works, and returns a mode `"P"` image. The palette's alpha values go to
`info["transparency"]`, so saving it as PNG keeps transparency:

```python
from PIL import Image
import libimgquant

img = Image.open("sprite.png")
small = libimgquant.quantize_image(img, max_colors=128, dithering=0.5)
small.save("sprite-128.png", optimize=True)
```

Pillow is imported only when `quantize_image` is called; the other functions don't need it.

## One palette for many images

For an animated GIF, a sprite sheet or a tile set, `quantize_frames` computes one palette
for several images of the same size and remaps each of them to it. Colors that appear in
only one frame still get palette entries.

```python
palette, frame_indices = libimgquant.quantize_frames(frames, width, height, max_colors=255)
# frame_indices[i]: the indices of frames[i] into the shared palette
```

## Options

All functions take the same keyword-only options:

| Option | Default | Meaning |
| --- | --- | --- |
| `max_colors` | `256` | At most this many palette entries, 2–256. |
| `speed` | `3` | 1 (slowest, best) to 10 (fastest). With `profile="improved"`, speeds 1–7 also order the palette for smaller PNGs (not in `quantize_frames`, whose palette is shared). |
| `quality` | `None` | Like JPEG quality, 0–100. An int is the target; `(minimum, target)` also sets a minimum. The fewest colors that reach the target are used; below the minimum, `QualityTooLowError` is raised. |
| `dithering` | `1.0` | Floyd-Steinberg dithering, 0 (none) to 1 (full). |
| `profile` | `"improved"` | `"improved"`: the version 3 algorithms. `"compat"`: libimagequant 2.4.1's algorithms, step by step, apart from a few bug fixes. |

## Errors

- `ValueError`: an option is out of range, the pixel buffer has the wrong size or is not
  8-bit, or the PNG is invalid.
- `libimgquant.QualityTooLowError` (a `ValueError`): the minimum quality can't be reached
  with `max_colors` colors. Keep the original image in that case, as pngquant does.
- `TypeError`: the pixels are not a bytes-like object.

```python
try:
    small = libimgquant.compress_png(data, quality=(80, 95))
except libimgquant.QualityTooLowError:
    small = data  # not worth converting
```

## Threads

Quantization runs on all cores (a rayon thread pool), and the GIL is released while it
runs, so other Python threads keep working and several images can be converted at once
from a `ThreadPoolExecutor`. The output does not depend on the number of threads.

## License

BSD-2-Clause, plus the original permissive notice by Jef Poskanzer. See the `LICENSE` file
in the package. The Rust library is [libimgquant on crates.io](https://crates.io/crates/libimgquant).
