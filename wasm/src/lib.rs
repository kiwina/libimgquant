//! WebAssembly bindings of libimgquant for the npm package `libimgquant`.
//!
//! The JS functions only convert values: the work, the option rules and the error messages
//! are in [`ops`] and [`options`], plain Rust that `cargo test` runs natively.

#![forbid(unsafe_code)]

pub mod ops;
pub mod options;

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array, Uint8ClampedArray};
use wasm_bindgen::prelude::*;

use options::{Settings, Value};

#[wasm_bindgen(typescript_custom_section)]
const TYPES: &str = r#"
/** Options of `quantize`, `compressPng` and `quantizeFrames`. Every option is optional. */
export interface Options {
  /** Maximum number of palette colors, 2-256. Default 256. */
  maxColors?: number;
  /** 1 (slowest, best) to 10 (fastest). Default 3. */
  speed?: number;
  /**
   * Quality range 0-100, like JPEG quality: use the fewest colors that reach `max`, and
   * throw if even `maxColors` colors can't reach `min`. `{min}` alone means up to 100,
   * `{max}` alone means from 0. Default: no limit, always `maxColors` colors at most.
   */
  quality?: { min?: number; max?: number } | [min: number, max: number];
  /** Floyd-Steinberg dithering level, 0 (none) to 1 (full); `true`/`false` are 1/0. Default 1. */
  dithering?: number | boolean;
  /** `'improved'` (the default, libimgquant 3) or `'compat'` (libimagequant 2.4.1's algorithms). */
  profile?: "improved" | "compat";
}

/** Pixel bytes: RGBA, 4 bytes per pixel, row by row (for example `ImageData.data`). */
export type Pixels = Uint8Array | Uint8ClampedArray | ArrayBuffer;

/** The result of `quantize`. */
export interface QuantizeResult {
  /** The palette as RGBA bytes, 4 per color; translucent colors come first. */
  palette: Uint8Array;
  /** One palette index per pixel, row by row. */
  indices: Uint8Array;
  /** The quality reached, 0-100, or `undefined` if it was not measured. */
  quality: number | undefined;
}

/** The result of `quantizeFrames`. */
export interface FramesResult {
  /** The shared palette as RGBA bytes, 4 per color. */
  palette: Uint8Array;
  /** Each frame's palette indices, one per pixel, in the order of the input frames. */
  frames: Uint8Array[];
}
"#;

#[wasm_bindgen]
extern "C" {
    /// The options object; typed for TypeScript, checked by [`Settings::parse`].
    #[wasm_bindgen(typescript_type = "Options")]
    pub type JsOptions;
}

/// Quantizes an RGBA image to at most 256 colors and remaps it to the palette.
///
/// Throws an `Error` if the sizes don't match the data, an option is invalid, or the
/// minimum `quality` can't be reached.
#[wasm_bindgen(js_name = quantize, unchecked_return_type = "QuantizeResult")]
pub fn js_quantize(
    #[wasm_bindgen(
        unchecked_param_type = "Pixels",
        param_description = "RGBA bytes, width * height * 4"
    )]
    rgba: JsValue,
    #[wasm_bindgen(param_description = "width in pixels")] width: f64,
    #[wasm_bindgen(param_description = "height in pixels")] height: f64,
    options: Option<JsOptions>,
) -> Result<JsValue, JsError> {
    let settings = settings(options)?;
    let rgba = bytes(&rgba, "rgba")?;
    let q = ops::quantize(&rgba, width, height, &settings).map_err(|e| JsError::new(&e))?;
    Ok(object(&[
        ("palette", Uint8Array::from(&q.palette[..]).into()),
        ("indices", Uint8Array::from(&q.indices[..]).into()),
        ("quality", q.quality.map_or(JsValue::UNDEFINED, JsValue::from)),
    ]))
}

/// Converts a PNG file (any color type and bit depth) to a palette PNG, like pngquant.
/// The result keeps the input's gamma and RGB ICC profile; the encoder picks the smallest
/// bit depth, and
/// the smaller of an unfiltered and a filtered encoding.
///
/// Throws an `Error` if the input is not a valid PNG, an option is invalid, or the
/// minimum `quality` can't be reached.
#[wasm_bindgen(js_name = compressPng)]
pub fn js_compress_png(
    #[wasm_bindgen(
        unchecked_param_type = "Uint8Array | ArrayBuffer",
        param_description = "the PNG file"
    )]
    png: JsValue,
    options: Option<JsOptions>,
) -> Result<Uint8Array, JsError> {
    let settings = settings(options)?;
    let png = bytes(&png, "png")?;
    let output = ops::compress_png(&png, &settings).map_err(|e| JsError::new(&e))?;
    Ok(Uint8Array::from(&output[..]))
}

/// Quantizes several RGBA images of the same size (animation frames, sprites, tiles) to
/// one shared palette and remaps each of them to it.
///
/// Every frame's colors count, weighted as `quantize` weights a single image, so colors
/// that appear in only one frame still get palette entries.
///
/// Throws an `Error` if there are no frames, a frame's length doesn't match the size, an
/// option is invalid, or the minimum `quality` can't be reached.
#[wasm_bindgen(js_name = quantizeFrames, unchecked_return_type = "FramesResult")]
pub fn js_quantize_frames(
    #[wasm_bindgen(unchecked_param_type = "Pixels[]", param_description = "RGBA bytes of each frame")] frames: JsValue,
    #[wasm_bindgen(param_description = "width of every frame in pixels")] width: f64,
    #[wasm_bindgen(param_description = "height of every frame in pixels")] height: f64,
    options: Option<JsOptions>,
) -> Result<JsValue, JsError> {
    let settings = settings(options)?;
    if !Array::is_array(&frames) {
        return Err(JsError::new("frames must be an array of Uint8Array"));
    }
    let frames = Array::from(&frames)
        .iter()
        .enumerate()
        .map(|(i, frame)| bytes(&frame, &format!("frame {i}")))
        .collect::<Result<Vec<_>, _>>()?;
    let q = ops::quantize_frames(&frames, width, height, &settings).map_err(|e| JsError::new(&e))?;
    let indices: Array = q
        .frames
        .iter()
        .map(|f| JsValue::from(Uint8Array::from(&f[..])))
        .collect();
    Ok(object(&[
        ("palette", Uint8Array::from(&q.palette[..]).into()),
        ("frames", indices.into()),
    ]))
}

fn settings(options: Option<JsOptions>) -> Result<Settings, JsError> {
    let value = options.map_or(Value::Undefined, |o| to_value(&o, 0));
    Settings::parse(&value).map_err(|e| JsError::new(&e))
}

/// Copies the bytes of a `Uint8Array` (or `Buffer`), `Uint8ClampedArray` or `ArrayBuffer`
/// into WebAssembly memory.
fn bytes(value: &JsValue, name: &str) -> Result<Vec<u8>, JsError> {
    if let Some(a) = value.dyn_ref::<Uint8Array>() {
        Ok(a.to_vec())
    } else if let Some(a) = value.dyn_ref::<Uint8ClampedArray>() {
        Ok(a.to_vec())
    } else if let Some(b) = value.dyn_ref::<ArrayBuffer>() {
        Ok(Uint8Array::new(b).to_vec())
    } else {
        Err(JsError::new(&format!(
            "{name} must be a Uint8Array, Uint8ClampedArray or ArrayBuffer"
        )))
    }
}

/// Converts an options value; objects and arrays only two levels deep (`quality`).
fn to_value(value: &JsValue, depth: u32) -> Value {
    if value.is_undefined() || value.is_null() {
        Value::Undefined
    } else if let Some(b) = value.as_bool() {
        Value::Bool(b)
    } else if let Some(n) = value.as_f64() {
        Value::Number(n)
    } else if let Some(s) = value.as_string() {
        Value::String(s)
    } else if Array::is_array(value) {
        // deeper levels only need their kind, for error messages
        let items = if depth < 2 {
            Array::from(value).iter().map(|v| to_value(&v, depth + 1)).collect()
        } else {
            Vec::new()
        };
        Value::Array(items)
    } else if depth >= 2 {
        Value::Other
    } else if value.is_object() && !value.is_function() {
        let fields = Object::entries(value.unchecked_ref())
            .iter()
            .map(|entry| {
                let entry = Array::from(&entry);
                let key = entry.get(0).as_string().unwrap_or_default();
                (key, to_value(&entry.get(1), depth + 1))
            })
            .collect();
        Value::Object(fields)
    } else {
        Value::Other
    }
}

/// A plain object with these properties.
fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let object = Object::new();
    for (name, value) in fields {
        // defining a property of a fresh plain object can't fail
        let _ = Reflect::set(&object, &JsValue::from_str(name), value);
    }
    object.into()
}
