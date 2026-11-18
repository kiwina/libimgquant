import sys
from collections.abc import Iterable
from typing import Literal, final

from typing_extensions import TypeAlias

if sys.version_info >= (3, 12):
    from collections.abc import Buffer
else:
    from typing_extensions import Buffer

__all__ = [
    "QualityTooLowError",
    "QuantizeResult",
    "__version__",
    "compress_png",
    "quantize",
    "quantize_frames",
]

__version__: str

_Color: TypeAlias = tuple[int, int, int, int]
"""A palette entry: (r, g, b, a), each 0-255, not premultiplied."""

_Quality: TypeAlias = int | tuple[int, int] | None
"""None (no limit), a target 0-100, or (minimum, target)."""

_Profile: TypeAlias = Literal["improved", "compat"]

class QualityTooLowError(ValueError):
    """The image could not be quantized within the minimum ``quality``."""

@final
class QuantizeResult:
    """The result of :func:`quantize`."""

    @property
    def palette(self) -> list[_Color]:
        """At most ``max_colors`` (r, g, b, a) entries, translucent entries first."""
    @property
    def indices(self) -> bytes:
        """One palette index per pixel, row by row (``width * height`` bytes)."""
    @property
    def quality(self) -> int | None:
        """The quality reached, 0-100, on the scale of the ``quality`` option."""
    @property
    def width(self) -> int: ...
    @property
    def height(self) -> int: ...

def quantize(
    pixels: Buffer,
    width: int,
    height: int,
    *,
    max_colors: int = 256,
    speed: int = 3,
    quality: _Quality = None,
    dithering: float = 1.0,
    profile: _Profile = "improved",
) -> QuantizeResult:
    """Quantizes ``width * height`` RGBA pixels (``width * height * 4`` bytes, row by row)
    to at most ``max_colors`` colors, and remaps them with Floyd-Steinberg ``dithering``
    (0 = none, 1 = full).

    ``pixels`` is any bytes-like object of 8-bit values: bytes, bytearray, memoryview,
    array.array("B") or a uint8 numpy array such as one of shape (height, width, 4).

    ``speed`` goes from 1 (slowest, best) to 10 (fastest). ``quality`` works like JPEG
    quality: the fewest colors that reach the target are used, and below the minimum
    :class:`QualityTooLowError` is raised. ``profile="compat"`` reproduces libimagequant
    2.4.1's algorithms, apart from a few bug fixes.

    Raises ValueError for arguments out of range, TypeError for objects without the buffer
    protocol, and QualityTooLowError (a ValueError) when the minimum quality can't be
    reached.
    """

def compress_png(
    data: Buffer,
    *,
    max_colors: int = 256,
    speed: int = 3,
    quality: _Quality = None,
    dithering: float = 1.0,
    profile: _Profile = "improved",
) -> bytes:
    """Converts a PNG file to a palette PNG, like pngquant, and returns the new file.

    Any PNG can be read; the output keeps its dimensions, gamma and RGB ICC profile, uses the
    smallest bit depth,
    a tRNS chunk only as long as needed, and the smaller of a filtered and an unfiltered
    encoding. Options and errors as for :func:`quantize`; ValueError for an invalid PNG.
    """

def quantize_frames(
    frames: Iterable[Buffer],
    width: int,
    height: int,
    *,
    max_colors: int = 256,
    speed: int = 3,
    quality: _Quality = None,
    dithering: float = 1.0,
    profile: _Profile = "improved",
) -> tuple[list[_Color], list[bytes]]:
    """Quantizes several RGBA images of the same size to one palette, for example the
    frames of an animated GIF, and returns ``(palette, indices)`` with the indices of each
    frame. Each frame is weighted as :func:`quantize` would weight it on its own.
    Options and errors as for :func:`quantize`.
    """
