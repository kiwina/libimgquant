"""Pure-Rust, BSD-licensed color quantizer: converts RGBA images to 8-bit palette images
with alpha, for small PNGs and GIFs.

- :func:`quantize` turns RGBA pixels into a palette and one palette index per pixel.
- :func:`compress_png` converts a PNG file to a small palette PNG, like pngquant.
- :func:`quantize_frames` finds one palette for several images, such as animation frames.
- :func:`quantize_image` does the same as :func:`quantize` for a Pillow image.

All of them release the GIL while they work.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Literal

from ._libimgquant import (
    QualityTooLowError,
    QuantizeResult,
    __version__,
    compress_png,
    quantize,
    quantize_frames,
)

if TYPE_CHECKING:
    from PIL import Image

__all__ = [
    "QualityTooLowError",
    "QuantizeResult",
    "__version__",
    "compress_png",
    "quantize",
    "quantize_frames",
    "quantize_image",
]


def quantize_image(
    img: Image.Image,
    *,
    max_colors: int = 256,
    speed: int = 3,
    quality: int | tuple[int, int] | None = None,
    dithering: float = 1.0,
    profile: Literal["improved", "compat"] = "improved",
) -> Image.Image:
    """Quantizes a Pillow image and returns it in mode ``"P"``.

    Images in other modes are converted to RGBA first. Alpha is kept: the palette's alpha
    values go to ``info["transparency"]`` (up to the last translucent entry, like a PNG
    tRNS chunk), which ``Image.save`` writes to PNG files. The options are those of
    :func:`quantize`. Pillow is imported only when this function is called.
    """
    from PIL import Image

    rgba = img if img.mode == "RGBA" else img.convert("RGBA")
    width, height = rgba.size
    result = quantize(
        rgba.tobytes(),
        width,
        height,
        max_colors=max_colors,
        speed=speed,
        quality=quality,
        dithering=dithering,
        profile=profile,
    )
    palette = result.palette
    out = Image.frombytes("P", (width, height), result.indices)
    out.putpalette(bytes(c for r, g, b, _ in palette for c in (r, g, b)), "RGB")
    # translucent entries come first, so this is usually short or empty
    alphas = [a for _, _, _, a in palette]
    last_translucent = max((i for i, a in enumerate(alphas) if a < 255), default=-1)
    if last_translucent >= 0:
        out.info["transparency"] = bytes(alphas[: last_translucent + 1])
    return out
