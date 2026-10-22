"""Generated test images; no image files live in the repository."""

import os
import random
import shutil
from pathlib import Path

import pytest


def gradient(width, height):
    """RGBA bytes: a smooth two-axis gradient with a translucent band at the top and a
    fully transparent corner, which needs many colors and all kinds of alpha."""
    out = bytearray()
    for y in range(height):
        for x in range(width):
            a = 0 if x < 4 and y < 4 else 100 if y < height // 6 else 255
            out += bytes(((x * 255) // max(width - 1, 1), (y * 255) // max(height - 1, 1), 128, a))
    return bytes(out)


def textured(width, height, seed=1):
    """Like gradient(), with some grain, so that a lossless PNG of it is large, like that
    of a photo."""
    rng = random.Random(seed)
    out = bytearray(gradient(width, height))
    for i in range(0, len(out), 4):
        for c in range(i, i + 3):
            out[c] = min(255, max(0, out[c] + rng.randint(-12, 12)))
    return bytes(out)


def noise(width, height, seed=1):
    """Opaque RGBA noise, the slowest kind of image to quantize."""
    rng = random.Random(seed)
    out = bytearray(rng.getrandbits(8) for _ in range(width * height * 4))
    out[3::4] = b"\xff" * (width * height)
    return bytes(out)


def flat(width, height, colors):
    """RGBA bytes of vertical stripes in the given (r, g, b, a) colors."""
    return b"".join(bytes(colors[x * len(colors) // width]) for _ in range(height) for x in range(width))


@pytest.fixture
def corpus_png(tmp_path):
    """A real PNG, copied into the test's temporary directory: the file named by
    LIBIMGQUANT_TEST_PNG, for example an image of the project's test corpus."""
    source = os.environ.get("LIBIMGQUANT_TEST_PNG")
    if not source or not Path(source).is_file():
        pytest.skip("set LIBIMGQUANT_TEST_PNG to a PNG file to run this test")
    return Path(shutil.copy(source, tmp_path / Path(source).name))
