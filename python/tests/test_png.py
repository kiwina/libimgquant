import io
import subprocess
import sys

import numpy as np
import pytest
from conftest import gradient, noise, textured
from PIL import Image

import libimgquant


def rgba_png(pixels, width, height):
    out = io.BytesIO()
    Image.frombytes("RGBA", (width, height), pixels).save(out, "PNG")
    return out.getvalue()


def as_rgba(data):
    return np.asarray(Image.open(io.BytesIO(data)).convert("RGBA"), dtype=np.int32)


def premultiplied(data):
    """RGBA with color scaled by alpha, so invisible pixels compare equal whatever their color."""
    rgba = as_rgba(data)
    rgba[..., :3] = rgba[..., :3] * rgba[..., 3:] // 255
    return rgba


def test_compress_png_round_trip():
    width, height = 200, 150
    original = rgba_png(textured(width, height), width, height)
    small = libimgquant.compress_png(original, quality=(50, 95))
    assert isinstance(small, bytes)
    assert len(small) < len(original)
    with Image.open(io.BytesIO(small)) as img:
        assert img.mode == "P"
        assert img.size == (width, height)
    error = np.abs(as_rgba(small) - as_rgba(original))
    assert error.mean() < 6
    # alpha is kept: the fully transparent corner, the translucent band, the opaque rest
    alpha = as_rgba(small)[..., 3]
    assert (alpha[:4, :4] == 0).all()
    assert (np.abs(alpha[10, 4:] - 100) <= 2).all()
    assert (alpha[height // 6 :] == 255).all()


def test_compress_png_small_palette_uses_low_bit_depth():
    width, height = 64, 64
    original = rgba_png(gradient(width, height), width, height)
    small = libimgquant.compress_png(original, max_colors=4, dithering=0)
    with Image.open(io.BytesIO(small)) as img:
        assert img.mode == "P"
        assert len(img.getpalette()) // 3 <= 4


def test_compress_png_corpus(corpus_png):
    data = corpus_png.read_bytes()
    small = libimgquant.compress_png(data)
    with Image.open(corpus_png) as before, Image.open(io.BytesIO(small)) as after:
        assert after.mode == "P"
        assert after.size == before.size
        # a palette PNG may already be as small as it gets
        if before.mode != "P":
            assert len(small) < len(data)
    error = np.abs(premultiplied(small) - premultiplied(data))
    assert error.mean() < 4


def test_compress_png_errors():
    with pytest.raises(ValueError, match="invalid PNG"):
        libimgquant.compress_png(b"not a png")
    width, height = 64, 64
    original = rgba_png(gradient(width, height), width, height)
    with pytest.raises(libimgquant.QualityTooLowError):
        libimgquant.compress_png(original, max_colors=2, quality=(80, 100))
    with pytest.raises(ValueError, match="speed"):
        libimgquant.compress_png(original, speed=0)
    # any bytes-like object works
    assert libimgquant.compress_png(bytearray(original)) == libimgquant.compress_png(original)


def test_quantize_frames():
    width, height = 32, 24
    frames = [gradient(width, height), noise(width, height, seed=2), bytes([200, 30, 30, 255]) * (width * height)]
    palette, indices = libimgquant.quantize_frames(frames, width, height, max_colors=64)
    assert 1 <= len(palette) <= 64
    assert len(indices) == len(frames)
    for frame in indices:
        assert isinstance(frame, bytes)
        assert len(frame) == width * height
        assert max(frame) < len(palette)
    # the color of the flat frame, used only there, still gets a close palette entry
    r, g, b, a = palette[indices[2][0]]
    assert abs(r - 200) + abs(g - 30) + abs(b - 30) <= 6 and a == 255


def test_quantize_frames_exact_and_numpy():
    width, height = 16, 8
    reds = np.zeros((height, width, 4), np.uint8)
    reds[..., 0], reds[..., 3] = np.arange(width) * 16, 255
    blues = np.zeros((height, width, 4), np.uint8)
    blues[..., 2], blues[..., 3] = np.arange(width)[:, None].T * 16, 255
    palette, indices = libimgquant.quantize_frames(iter([reds, blues]), width, height, dithering=0)
    lut = np.array(palette, np.uint8)
    for frame, idx in zip((reds, blues), indices):
        restored = lut[np.frombuffer(idx, np.uint8)].reshape(height, width, 4)
        assert (restored == frame).all()


def test_quantize_frames_errors():
    with pytest.raises(ValueError, match="empty"):
        libimgquant.quantize_frames([], 4, 4)
    with pytest.raises(ValueError, match=r"frames\[1\]"):
        libimgquant.quantize_frames([bytes(64), bytes(60)], 4, 4)
    with pytest.raises(TypeError, match="iterable"):
        libimgquant.quantize_frames(5, 4, 4)
    with pytest.raises(libimgquant.QualityTooLowError):
        libimgquant.quantize_frames([gradient(64, 64)], 64, 64, max_colors=2, quality=(80, 100))


def test_quantize_image_keeps_alpha(tmp_path):
    width, height = 60, 40
    img = Image.frombytes("RGBA", (width, height), gradient(width, height))
    out = libimgquant.quantize_image(img, max_colors=32)
    assert out.mode == "P"
    assert out.size == img.size
    assert len(out.getpalette()) // 3 <= 32
    transparency = out.info["transparency"]
    assert isinstance(transparency, bytes) and 0 in transparency
    for restored in (out.convert("RGBA"), _saved(out, tmp_path)):
        alpha = np.asarray(restored)[..., 3]
        assert (alpha[:4, :4] == 0).all()
        assert (np.abs(alpha[5, 4:].astype(int) - 100) <= 2).all()
        assert (alpha[height // 6 :] == 255).all()


def _saved(img, tmp_path):
    path = tmp_path / "out.png"
    img.save(path)
    return Image.open(path).convert("RGBA")


def test_quantize_image_other_modes():
    rgb = Image.frombytes("RGBA", (30, 20), gradient(30, 20)).convert("RGB")
    out = libimgquant.quantize_image(rgb, max_colors=16, dithering=0, speed=1)
    assert out.mode == "P"
    assert "transparency" not in out.info
    assert np.abs(np.asarray(out.convert("RGB"), int) - np.asarray(rgb, int)).mean() < 12
    gray = Image.linear_gradient("L")
    assert libimgquant.quantize_image(gray).size == gray.size


def test_pillow_is_imported_lazily(tmp_path):
    code = "import sys, libimgquant; assert 'PIL' not in sys.modules"
    # not from python/, where `-c` would import the source folder instead of the package
    subprocess.run([sys.executable, "-c", code], check=True, cwd=tmp_path)
