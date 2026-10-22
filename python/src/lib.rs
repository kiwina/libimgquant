//! Python bindings of libimgquant: the native module `libimgquant._libimgquant`, which
//! `python/libimgquant/__init__.py` re-exports together with the pure-Python helpers.
//!
//! Every function copies or borrows its input while holding the GIL, then quantizes with
//! the GIL released, so other Python threads keep running.

use libimgquant::png_io::{self, PngError};
use libimgquant::{Attributes, Error, Histogram, Profile, RGBA};
use pyo3::create_exception;
use pyo3::exceptions::{PyMemoryError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyMemoryView};
use rgb::FromSlice;

create_exception!(
    libimgquant,
    QualityTooLowError,
    PyValueError,
    "The image could not be quantized within the minimum `quality`."
);

/// Quantization settings shared by every function, validated with Python-style messages.
struct Options {
    attr: Attributes,
    dithering: f32,
}

impl Options {
    fn new(
        max_colors: i64,
        speed: i64,
        quality: Option<&Bound<'_, PyAny>>,
        dithering: f64,
        profile: &str,
    ) -> PyResult<Self> {
        let mut attr = Attributes::new();
        // set_profile re-derives the speed presets, so it comes before set_speed
        attr.set_profile(match profile.to_ascii_lowercase().as_str() {
            "improved" => Profile::Improved,
            "compat" => Profile::Compat,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "profile must be 'improved' or 'compat', got {profile:?}"
                )));
            }
        });
        let max_colors = u32::try_from(max_colors)
            .ok()
            .filter(|c| (2..=256).contains(c))
            .ok_or_else(|| PyValueError::new_err(format!("max_colors must be 2-256, got {max_colors}")))?;
        attr.set_max_colors(max_colors).map_err(quantize_error)?;
        let speed = i32::try_from(speed)
            .ok()
            .filter(|s| (1..=10).contains(s))
            .ok_or_else(|| PyValueError::new_err(format!("speed must be 1-10, got {speed}")))?;
        attr.set_speed(speed).map_err(quantize_error)?;
        if let Some(quality) = quality {
            let (minimum, target) = quality_range(quality)?;
            attr.set_quality(minimum, target).map_err(quantize_error)?;
        }
        if !(0.0..=1.0).contains(&dithering) {
            return Err(PyValueError::new_err(format!("dithering must be 0-1, got {dithering}")));
        }
        Ok(Self {
            attr,
            dithering: dithering as f32,
        })
    }
}

/// `quality` as `(minimum, target)`: an int is the target with no minimum, like
/// `imgquant --quality 80`; a pair is `(minimum, target)`.
fn quality_range(quality: &Bound<'_, PyAny>) -> PyResult<(u8, u8)> {
    let pair = match quality.extract::<i64>() {
        Ok(target) => Some((0, target)),
        Err(_) => match quality.extract::<Vec<i64>>().as_deref() {
            Ok(&[minimum, target]) => Some((minimum, target)),
            _ => None,
        },
    };
    let (minimum, target) = pair
        .ok_or_else(|| PyTypeError::new_err("quality must be None, an int (the target) or a (minimum, target) pair"))?;
    let in_range = |q: i64| u8::try_from(q).ok().filter(|&q| q <= 100);
    match (in_range(minimum), in_range(target)) {
        (Some(minimum), Some(target)) if minimum <= target => Ok((minimum, target)),
        _ => Err(PyValueError::new_err(format!(
            "quality must be 0-100 with minimum <= target, got ({minimum}, {target})"
        ))),
    }
}

fn quantize_error(e: Error) -> PyErr {
    match e {
        Error::QualityTooLow => QualityTooLowError::new_err(e.to_string()),
        Error::OutOfMemory => PyMemoryError::new_err(e.to_string()),
        _ => PyValueError::new_err(e.to_string()),
    }
}

fn png_error(e: PngError) -> PyErr {
    match e {
        PngError::Quantize(e) => quantize_error(e),
        PngError::Decode(_) => PyValueError::new_err(e.to_string()),
        _ => PyRuntimeError::new_err(e.to_string()),
    }
}

/// The bytes of a bytes-like object. `bytes` is borrowed as it is; anything else with the
/// buffer protocol (bytearray, memoryview, numpy arrays, array.array) is copied in C order
/// with `memoryview.tobytes()`. The buffer protocol is not in the stable ABI before Python
/// 3.11, and a mutable buffer could change while the GIL is released.
fn to_bytes<'py>(obj: &Bound<'py, PyAny>, name: &str) -> PyResult<Bound<'py, PyBytes>> {
    if let Ok(bytes) = obj.cast::<PyBytes>() {
        return Ok(bytes.clone());
    }
    let view = PyMemoryView::from(obj).map_err(|_| {
        PyTypeError::new_err(format!(
            "{name} must be a bytes-like object (bytes, bytearray, memoryview, numpy array), not {}",
            obj.get_type().name().map_or_else(|_| "?".to_owned(), |n| n.to_string())
        ))
    })?;
    let format: String = view.getattr("format")?.extract()?;
    if !matches!(format.trim_start_matches(['@', '=', '<', '>', '!']), "B" | "c") {
        return Err(PyValueError::new_err(format!(
            "{name} must hold 8-bit unsigned values (uint8), got buffer format {format:?}"
        )));
    }
    Ok(view.call_method0("tobytes")?.cast_into::<PyBytes>()?)
}

/// `width * height` RGBA pixels of a bytes-like object.
fn rgba_bytes<'py>(obj: &Bound<'py, PyAny>, name: &str, width: usize, height: usize) -> PyResult<Bound<'py, PyBytes>> {
    let bytes = to_bytes(obj, name)?;
    let expected = width.checked_mul(height).and_then(|n| n.checked_mul(4));
    if expected != Some(bytes.as_bytes().len()) {
        return Err(PyValueError::new_err(format!(
            "{name} must have width * height * 4 = {} bytes (RGBA), got {}",
            expected.map_or_else(|| "too many".to_owned(), |n| n.to_string()),
            bytes.as_bytes().len()
        )));
    }
    Ok(bytes)
}

fn image_size(width: i64, height: i64) -> PyResult<(usize, usize)> {
    match (usize::try_from(width), usize::try_from(height)) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(PyValueError::new_err(format!(
            "width and height must be positive, got {width}x{height}"
        ))),
    }
}

fn palette_tuples(palette: &[RGBA]) -> Vec<(u8, u8, u8, u8)> {
    palette.iter().map(|c| (c.r, c.g, c.b, c.a)).collect()
}

/// The result of `quantize`: the palette, one palette index per pixel, and the quality
/// reached.
#[pyclass(frozen, module = "libimgquant")]
struct QuantizeResult {
    palette: Vec<(u8, u8, u8, u8)>,
    /// One palette index per pixel, row by row.
    #[pyo3(get)]
    indices: Py<PyBytes>,
    /// The quality reached, 0-100 (as for the `quality` option).
    #[pyo3(get)]
    quality: Option<u8>,
    /// Width in pixels.
    #[pyo3(get)]
    width: usize,
    /// Height in pixels.
    #[pyo3(get)]
    height: usize,
}

#[pymethods]
impl QuantizeResult {
    /// The palette as `(r, g, b, a)` tuples, at most `max_colors`. Translucent entries
    /// come first, so a PNG tRNS chunk can stop after the last one.
    #[getter]
    fn palette(&self) -> Vec<(u8, u8, u8, u8)> {
        self.palette.clone()
    }

    fn __repr__(&self) -> String {
        let quality = self.quality.map_or_else(|| "None".to_owned(), |q| q.to_string());
        format!(
            "QuantizeResult(width={}, height={}, colors={}, quality={quality})",
            self.width,
            self.height,
            self.palette.len()
        )
    }
}

/// Quantizes `width * height` RGBA pixels to at most `max_colors` colors and remaps them.
///
/// `pixels` is any bytes-like object of `width * height * 4` bytes, row by row: bytes,
/// bytearray, memoryview, array.array("B") or a uint8 numpy array such as one of shape
/// (height, width, 4).
///
/// Options: `speed` 1 (slowest, best) to 10 (fastest); `quality` None, a target 0-100 or
/// (minimum, target), like JPEG quality: the fewest colors that reach the target are used,
/// and QualityTooLowError is raised below the minimum; `dithering` 0 (none) to 1 (full
/// Floyd-Steinberg); `profile` "improved" or "compat" (libimagequant 2.4.1's algorithms).
///
/// Raises ValueError for arguments out of range, TypeError for objects without the buffer
/// protocol, and QualityTooLowError (a ValueError) when the minimum quality can't be
/// reached. The GIL is released while quantizing.
#[pyfunction]
#[pyo3(signature = (pixels, width, height, *, max_colors=256, speed=3, quality=None, dithering=1.0, profile="improved"))]
#[allow(clippy::too_many_arguments)]
fn quantize(
    py: Python<'_>,
    pixels: &Bound<'_, PyAny>,
    width: i64,
    height: i64,
    max_colors: i64,
    speed: i64,
    quality: Option<&Bound<'_, PyAny>>,
    dithering: f64,
    profile: &str,
) -> PyResult<QuantizeResult> {
    let options = Options::new(max_colors, speed, quality, dithering, profile)?;
    let (width, height) = image_size(width, height)?;
    let pixels = rgba_bytes(pixels, "pixels", width, height)?;
    let pixels = pixels.as_bytes().as_rgba();
    let (palette, indices, quality) = py
        .detach(|| {
            let attr = &options.attr;
            let mut image = attr.new_image_borrowed(pixels, width, height, 0.0)?;
            let mut result = attr.quantize(&mut image)?;
            result.set_dithering_level(options.dithering)?;
            let (palette, indices) = result.remapped(&mut image)?;
            Ok((palette, indices, result.quantization_quality()))
        })
        .map_err(quantize_error)?;
    Ok(QuantizeResult {
        palette: palette_tuples(&palette),
        indices: PyBytes::new(py, &indices).unbind(),
        quality,
        width,
        height,
    })
}

/// Converts a PNG file (any bytes-like object) to a palette PNG, like pngquant, and
/// returns the new file.
///
/// Any PNG can be read. The output keeps its size and gamma, and uses the smallest bit
/// depth, a tRNS chunk only as long as needed and the smaller of a filtered and an
/// unfiltered encoding. Options and errors as for `quantize`; ValueError for an invalid
/// PNG.
#[pyfunction]
#[pyo3(signature = (data, *, max_colors=256, speed=3, quality=None, dithering=1.0, profile="improved"))]
fn compress_png<'py>(
    py: Python<'py>,
    data: &Bound<'py, PyAny>,
    max_colors: i64,
    speed: i64,
    quality: Option<&Bound<'py, PyAny>>,
    dithering: f64,
    profile: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    let options = Options::new(max_colors, speed, quality, dithering, profile)?;
    let data = to_bytes(data, "data")?;
    let data = data.as_bytes();
    let (output, _) = py
        .detach(|| png_io::compress_png(data, &options.attr, options.dithering))
        .map_err(png_error)?;
    Ok(PyBytes::new(py, &output))
}

/// Quantizes several RGBA images of the same size to one palette, for example the frames
/// of an animated GIF, and returns `(palette, indices)` with the indices of each frame.
///
/// `frames` is an iterable of bytes-like objects as for `quantize`. Each frame is weighted
/// as `quantize` would weight it on its own, so colors that appear in only one frame still
/// get palette entries. Options and errors as for `quantize`.
#[pyfunction]
#[pyo3(signature = (frames, width, height, *, max_colors=256, speed=3, quality=None, dithering=1.0, profile="improved"))]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn quantize_frames<'py>(
    py: Python<'py>,
    frames: &Bound<'py, PyAny>,
    width: i64,
    height: i64,
    max_colors: i64,
    speed: i64,
    quality: Option<&Bound<'py, PyAny>>,
    dithering: f64,
    profile: &str,
) -> PyResult<(Vec<(u8, u8, u8, u8)>, Vec<Bound<'py, PyBytes>>)> {
    let options = Options::new(max_colors, speed, quality, dithering, profile)?;
    let (width, height) = image_size(width, height)?;
    let frames = frames
        .try_iter()
        .map_err(|_| PyTypeError::new_err("frames must be an iterable of bytes-like objects"))?
        .enumerate()
        .map(|(i, frame)| rgba_bytes(&frame?, &format!("frames[{i}]"), width, height))
        .collect::<PyResult<Vec<_>>>()?;
    if frames.is_empty() {
        return Err(PyValueError::new_err("frames must not be empty"));
    }
    let pixels: Vec<&[RGBA]> = frames.iter().map(|f| f.as_bytes().as_rgba()).collect();
    let (palette, indices) = py
        .detach(|| {
            let attr = &options.attr;
            let mut images = pixels
                .iter()
                .map(|p| attr.new_image_borrowed(p, width, height, 0.0))
                .collect::<Result<Vec<_>, Error>>()?;
            let mut histogram = Histogram::new(attr);
            for image in &mut images {
                histogram.add_image(attr, image)?;
            }
            let mut result = histogram.quantize(attr)?;
            result.set_dithering_level(options.dithering)?;
            let indices = images
                .iter_mut()
                .map(|image| result.remapped(image).map(|(_, indices)| indices))
                .collect::<Result<Vec<_>, Error>>()?;
            Ok((result.palette_vec(), indices))
        })
        .map_err(quantize_error)?;
    let indices = indices.iter().map(|i| PyBytes::new(py, i)).collect();
    Ok((palette_tuples(&palette), indices))
}

/// Pure-Rust, BSD-licensed color quantizer: converts RGBA images to 8-bit palette images
/// with alpha, for small PNGs and GIFs.
#[pymodule]
mod _libimgquant {
    #[pymodule_export]
    use super::{QualityTooLowError, QuantizeResult, compress_png, quantize, quantize_frames};

    use pyo3::prelude::*;

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))
    }
}
