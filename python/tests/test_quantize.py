import array
import importlib.metadata
import math
import threading
import time

import numpy as np
import pytest
from conftest import flat, gradient, noise

import libimgquant


def check_result(result, width, height, max_colors=256):
    palette = result.palette
    assert 1 <= len(palette) <= max_colors
    assert all(len(c) == 4 and all(0 <= v <= 255 for v in c) for c in palette)
    assert isinstance(result.indices, bytes)
    assert len(result.indices) == width * height
    assert max(result.indices) < len(palette)
    assert (result.width, result.height) == (width, height)
    # translucent entries come first
    alphas = [c[3] for c in palette]
    opaque = alphas.index(255) if 255 in alphas else len(alphas)
    assert all(a < 255 for a in alphas[:opaque]) and all(a == 255 for a in alphas[opaque:])


def unpack(result):
    """The remapped image as RGBA bytes."""
    return b"".join(bytes(result.palette[i]) for i in result.indices)


def test_version():
    assert libimgquant.__version__ == importlib.metadata.version("libimgquant")


def test_quantize_bytes():
    width, height = 64, 48
    result = libimgquant.quantize(gradient(width, height), width, height)
    check_result(result, width, height)
    assert 0 <= result.quality <= 100
    assert any(c[3] == 0 for c in result.palette)
    assert any(0 < c[3] < 255 for c in result.palette)
    assert "QuantizeResult(width=64, height=48" in repr(result)


def test_exact_colors_are_kept():
    colors = [(255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 128), (10, 20, 30, 255)]
    pixels = flat(40, 10, colors)
    result = libimgquant.quantize(pixels, 40, 10, dithering=0)
    assert sorted(result.palette) == sorted(colors)
    assert unpack(result) == pixels


def test_numpy_input():
    width, height = 50, 40
    pixels = gradient(width, height)
    array3d = np.frombuffer(pixels, np.uint8).reshape(height, width, 4)
    expected = libimgquant.quantize(pixels, width, height)
    for arr in (array3d, array3d.reshape(-1), array3d.copy()):
        result = libimgquant.quantize(arr, width, height)
        assert result.palette == expected.palette
        assert result.indices == expected.indices
    # a strided view is read in C order, like its contiguous copy
    wide = np.zeros((height, width * 2, 4), np.uint8)
    wide[:, ::2] = array3d
    view = wide[:, ::2]
    assert not view.flags.c_contiguous
    assert libimgquant.quantize(view, width, height).indices == expected.indices


def test_other_buffers():
    width, height = 20, 10
    pixels = gradient(width, height)
    expected = libimgquant.quantize(pixels, width, height).indices
    for buf in (bytearray(pixels), memoryview(pixels), array.array("B", pixels)):
        assert libimgquant.quantize(buf, width, height).indices == expected


def test_max_colors_and_speed():
    width, height = 64, 64
    pixels = gradient(width, height)
    for max_colors in (2, 16, 255):
        for speed in (1, 5, 10):
            result = libimgquant.quantize(pixels, width, height, max_colors=max_colors, speed=speed)
            check_result(result, width, height, max_colors)


def test_quality():
    width, height = 64, 64
    pixels = gradient(width, height)
    best = libimgquant.quantize(pixels, width, height, quality=100)
    low = libimgquant.quantize(pixels, width, height, quality=(0, 30))
    assert len(low.palette) < len(best.palette)
    assert low.quality <= best.quality
    ok = libimgquant.quantize(pixels, width, height, quality=[50, 90])
    assert ok.quality >= 50


def test_quality_too_low():
    width, height = 64, 64
    with pytest.raises(libimgquant.QualityTooLowError) as info:
        libimgquant.quantize(gradient(width, height), width, height, max_colors=2, quality=(80, 100))
    assert isinstance(info.value, ValueError)


def test_dithering_and_profile():
    width, height = 64, 64
    pixels = gradient(width, height)
    plain = libimgquant.quantize(pixels, width, height, max_colors=16, dithering=0)
    dithered = libimgquant.quantize(pixels, width, height, max_colors=16, dithering=1)
    assert plain.indices != dithered.indices
    compat = libimgquant.quantize(pixels, width, height, max_colors=16, profile="compat")
    check_result(compat, width, height, 16)
    assert libimgquant.quantize(pixels, width, height, profile="Compat").indices == (
        libimgquant.quantize(pixels, width, height, profile="compat").indices
    )


def test_deterministic_across_threads():
    from concurrent.futures import ThreadPoolExecutor

    inputs = [(gradient(48 + i, 32), 48 + i, 32) for i in range(6)]
    sequential = [libimgquant.quantize(*args).indices for args in inputs]
    with ThreadPoolExecutor(4) as pool:
        parallel = list(pool.map(lambda args: libimgquant.quantize(*args).indices, inputs))
    assert parallel == sequential


def test_gil_is_released():
    width, height = 1024, 1024
    pixels = noise(width, height)
    counter = 0
    stop = False

    def count():
        nonlocal counter
        while not stop:
            counter += 1

    thread = threading.Thread(target=count)
    thread.start()
    try:
        # how fast the thread counts while this thread sleeps
        before, start = counter, time.perf_counter()
        time.sleep(0.2)
        idle_rate = (counter - before) / (time.perf_counter() - start)
        before, start = counter, time.perf_counter()
        libimgquant.quantize(pixels, width, height, speed=1)
        elapsed = time.perf_counter() - start
        busy_rate = (counter - before) / elapsed
    finally:
        stop = True
        thread.join()
    # With the GIL held for the whole call, the thread could only count in the switch
    # interval before the call (about 1% of the idle rate in a 0.7 s call); with it
    # released, it shares the CPU with the quantizer's worker threads.
    assert elapsed > 0.1, "the image should take a while to quantize"
    assert busy_rate > 0.05 * idle_rate, f"counted {busy_rate:.0f}/s during the call, {idle_rate:.0f}/s idle"


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"max_colors": 1}, "max_colors"),
        ({"max_colors": 257}, "max_colors"),
        ({"speed": 0}, "speed"),
        ({"speed": 11}, "speed"),
        ({"quality": 101}, "quality"),
        ({"quality": -1}, "quality"),
        ({"quality": (90, 50)}, "quality"),
        ({"dithering": 1.5}, "dithering"),
        ({"dithering": -0.1}, "dithering"),
        ({"dithering": math.nan}, "dithering"),
        ({"profile": "fast"}, "profile"),
    ],
)
def test_bad_options(kwargs, message):
    with pytest.raises(ValueError, match=message):
        libimgquant.quantize(gradient(8, 8), 8, 8, **kwargs)


def test_bad_pixels():
    pixels = gradient(8, 8)
    with pytest.raises(ValueError, match="width and height"):
        libimgquant.quantize(pixels, 0, 8)
    with pytest.raises(ValueError, match="width and height"):
        libimgquant.quantize(pixels, -8, -8)
    with pytest.raises(ValueError, match="256 bytes"):
        libimgquant.quantize(pixels[:-1], 8, 8)
    with pytest.raises(ValueError, match="288 bytes"):
        libimgquant.quantize(pixels, 8, 9)
    with pytest.raises(ValueError, match="uint8"):
        libimgquant.quantize(np.zeros((8, 8, 4), np.float32), 8, 8)
    with pytest.raises(ValueError, match="uint8"):
        libimgquant.quantize(array.array("I", [0] * 64), 8, 8)
    with pytest.raises(TypeError, match="bytes-like"):
        libimgquant.quantize("not pixels", 8, 8)
    with pytest.raises(TypeError, match="bytes-like"):
        libimgquant.quantize(list(pixels), 8, 8)
    with pytest.raises(TypeError, match="quality"):
        libimgquant.quantize(pixels, 8, 8, quality="high")
    with pytest.raises(TypeError):
        libimgquant.quantize(pixels, 8, 8, 256)  # options are keyword-only
