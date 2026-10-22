//! Converts 24/32-bit RGBA images to 8-bit palette images with alpha, for lossy PNG
//! compression and high-quality GIFs. Pure Rust, `#![forbid(unsafe_code)]`, BSD-2-Clause.
//!
//! libimgquant 3 starts from a faithful Rust rewrite of
//! [libimagequant](https://pngquant.org/lib/) 2.4.1, the last BSD-licensed release of the
//! library behind pngquant, and improves it with published color quantization research:
//! a variance-based median cut, Hartigan refinement after k-means, an exact nearest-color
//! search, an OKLab-shaped color difference and a palette order that compresses better.
//!
//! # Quick start
//!
//! ```
//! let (width, height) = (64, 48);
//! // Any RGBA pixels, row by row; here a gradient with a translucent band.
//! let pixels: Vec<libimgquant::RGBA> = (0..width * height)
//!     .map(|i| {
//!         let (x, y) = (i % width, i / width);
//!         libimgquant::RGBA::new((x * 4) as u8, (y * 5) as u8, 128, if y < 8 { 100 } else { 255 })
//!     })
//!     .collect();
//!
//! let mut attr = libimgquant::new();
//! attr.set_speed(3)?; // 1 (best) ..= 10 (fastest)
//! attr.set_quality(65, 90)?; // like JPEG quality: fail below 65, aim for 90
//! attr.set_max_colors(256)?;
//!
//! let mut image = attr.new_image(pixels, width, height, 0.0)?; // gamma 0 = sRGB
//! let mut result = attr.quantize(&mut image)?;
//! result.set_dithering_level(1.0)?; // the default is 0 (no dithering), as in 2.4.1
//!
//! let (palette, indices) = result.remapped(&mut image)?;
//! assert!(palette.len() <= 256);
//! assert_eq!(indices.len(), width * height);
//! // Translucent palette entries come first, so a PNG tRNS chunk can stop early.
//! assert!(palette[0].a < 255);
//! # Ok::<(), libimgquant::Error>(())
//! ```
//!
//! The steps:
//!
//! 1. [`Attributes`] (from [`new()`]) holds the settings: speed, quality range, number of
//!    colors and [`Profile`].
//! 2. [`Attributes::new_image`] and its variants wrap the pixels in an [`Image`]. Pixels can
//!    be owned, borrowed, given as separate rows, or produced row by row by a callback.
//! 3. [`Attributes::quantize`] computes the palette, returned as a [`QuantizationResult`].
//! 4. [`QuantizationResult::remapped`], [`remap_into`](QuantizationResult::remap_into) or
//!    [`remap_rows`](QuantizationResult::remap_rows) replace every pixel with a palette
//!    index, optionally with Floyd-Steinberg dithering. One result can remap several images
//!    with the same palette.
//!
//! For one palette that fits several images, such as the frames of an animated GIF, collect
//! their colors in a [`Histogram`] and quantize that instead of a single image.
//!
//! # Quality
//!
//! [`Attributes::set_quality`] takes a minimum and a target quality from 0 to 100. The
//! quantizer uses the fewest colors that reach the target. If even
//! [`max_colors`](Attributes::set_max_colors) colors stay below the minimum, quantization
//! fails with [`Error::QualityTooLow`], so the caller can keep the original image instead.
//! After remapping, [`QuantizationResult::quantization_quality`] reports the quality that
//! was reached.
//!
//! # Profiles
//!
//! | [`Profile`] | What it is |
//! | --- | --- |
//! | [`Improved`](Profile::Improved) (default) | The version 3 algorithms: better looking, smaller and faster output. |
//! | [`Compat`](Profile::Compat) | libimagequant 2.4.1 reproduced step by step, including its mixed `f32`/`f64` arithmetic, apart from a few bug fixes. Golden-hash tests freeze its output. |
//!
//! Select one with [`Attributes::set_profile`].
//!
//! # Cargo features
//!
//! - **`threads`** (default): runs the histogram, contrast maps, k-means, remapping and
//!   dithering in parallel with [rayon](https://crates.io/crates/rayon). Parallel sums use
//!   fixed work chunks, so the output is the same for any number of threads. It can differ in
//!   the last bits from the build without `threads`, which sums in exactly the original order.
//!   Use `default-features = false` for a single-threaded build without the rayon
//!   dependency.
//! - **`png`**: the `png_io` module, which decodes any PNG to RGBA and writes palette PNGs
//!   as described under [Writing PNGs](#writing-pngs), with the
//!   [png](https://crates.io/crates/png) crate. `png_io::compress_png` converts a PNG file
//!   in one call.
//!
//! Output is deterministic and identical on every platform: dithering noise comes from the
//! crate's own seeded generator, sorting uses a fixed tie order and hashing is little-endian.
//!
//! # Writing PNGs
//!
//! This crate computes the palette and the indices; any PNG encoder can write them, and with
//! the `png` feature, `png_io::encode_indexed` does as follows. For the smallest files:
//!
//! - **Bit depth:** pack palettes of up to 2, 4 or 16 colors into 1, 2 or 4 bits per pixel.
//! - **tRNS:** stop the chunk after the last translucent entry. Translucent entries always
//!   come first, so it is usually short or absent.
//! - **Filters:** encode twice and keep the smaller file, once with filter type None and once
//!   with per-row filters chosen by minimum entropy. The default profile orders the palette
//!   so that colors that touch in the image get close indices, which often makes the filtered
//!   encoding smaller. Adaptive filtering on its own, the png crate's default, makes palette
//!   images 15-20% larger.
//!
//! With the [png](https://crates.io/crates/png) crate (0.18), for 8 bits per pixel:
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use libimgquant::RGBA;
//!
//! fn encode(width: usize, height: usize, palette: &[RGBA], indices: &[u8], filter: png::Filter)
//!     -> Result<Vec<u8>, png::EncodingError>
//! {
//!     let mut out = Vec::new();
//!     let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
//!     encoder.set_color(png::ColorType::Indexed);
//!     encoder.set_depth(png::BitDepth::Eight);
//!     // set_compression also resets the filter, so it comes first
//!     encoder.set_compression(png::Compression::High);
//!     encoder.set_filter(filter);
//!     encoder.set_palette(palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect::<Vec<_>>());
//!     // tRNS up to the last translucent entry
//!     let trns_len = palette.iter().rposition(|c| c.a < 255).map_or(0, |i| i + 1);
//!     if trns_len > 0 {
//!         encoder.set_trns(palette[..trns_len].iter().map(|c| c.a).collect::<Vec<_>>());
//!     }
//!     let mut writer = encoder.write_header()?;
//!     writer.write_image_data(indices)?;
//!     writer.finish()?;
//!     Ok(out)
//! }
//!
//! let (width, height) = (32, 32);
//! let pixels: Vec<RGBA> = (0..width * height)
//!     .map(|i| RGBA::new((i % width * 8) as u8, (i / width * 8) as u8, 200, 255))
//!     .collect();
//! let attr = libimgquant::new();
//! let mut image = attr.new_image(pixels, width, height, 0.0)?;
//! let (palette, indices) = attr.quantize(&mut image)?.remapped(&mut image)?;
//!
//! let unfiltered = encode(width, height, &palette, &indices, png::Filter::NoFilter)?;
//! let filtered = encode(width, height, &palette, &indices, png::Filter::MinEntropy)?;
//! let png_file = if filtered.len() < unfiltered.len() { filtered } else { unfiltered };
//! assert_eq!(&png_file[1..4], b"PNG");
//! # Ok(())
//! # }
//! ```
//!
//! # C API and command-line tool
//!
//! - [libimgquant-capi](https://crates.io/crates/libimgquant-capi) builds `libimgquant.so`,
//!   `.dylib`, `.dll` and `.a` with the header `libimgquant.h`, which has the libimagequant
//!   2.4.x API. C and C++ programs written for 2.4.x only need a new include and
//!   `-limgquant`. All unsafe code is in that crate.
//! - [imgquant](https://crates.io/crates/imgquant) is a pngquant-style command-line tool
//!   built on this crate.
//!
//! # Differences from libimagequant 2.4.1
//!
//! Apart from the new algorithms of [`Profile::Improved`], both profiles fix bugs of 2.4.1:
//! garbage edge-map values for high-contrast pixels, wrong contrast maps for images over
//! about 4 megapixels, `set_dithering_level` checking the previous value instead of the new
//! one, a palette rounded with the previous gamma after `set_output_gamma`, and a recursion
//! in median cut that could overflow the stack. The
//! [README](https://crates.io/crates/libimgquant) lists the differences in detail and maps
//! the C functions to this API.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attr;
mod blur;
mod dithermap;
mod error;
mod hist;
mod histogram;
mod image;
mod kmeans;
mod mediancut;
mod nearest;
mod pal;
#[cfg(feature = "png")]
pub mod png_io;
mod quant;
mod remap;
mod reorder;
mod rng;

pub use attr::{Attributes, Profile};
pub use error::Error;
pub use histogram::{Histogram, HistogramEntry};
pub use image::{Image, RowCallback};
pub use quant::QuantizationResult;

/// 8-bit RGBA pixel, from the [`rgb`] crate: the input pixel type and the palette entry type.
///
/// Color channels are not premultiplied by alpha.
///
/// ```
/// let red = libimgquant::RGBA::new(255, 0, 0, 255);
/// let translucent = libimgquant::RGBA { a: 128, ..red };
/// assert_eq!(translucent.r, 255);
/// ```
pub type RGBA = rgb::RGBA8;

/// Creates [`Attributes`] with default settings; the same as [`Attributes::new`].
///
/// ```
/// let attr = libimgquant::new();
/// assert_eq!(attr.max_colors(), 256);
/// assert_eq!(attr.speed(), 3);
/// ```
#[must_use]
pub fn new() -> Attributes {
    Attributes::new()
}
