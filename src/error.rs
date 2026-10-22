use std::fmt;

/// Errors reported by the library.
///
/// ```
/// let mut attr = libimgquant::new();
/// assert_eq!(attr.set_speed(11), Err(libimgquant::Error::ValueOutOfRange));
/// assert_eq!(libimgquant::Error::ValueOutOfRange.to_string(), "value out of range");
/// ```
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// The image could not be quantized within the configured minimum quality.
    QualityTooLow,
    /// An argument was outside of its documented range.
    ValueOutOfRange,
    /// A large buffer could not be allocated.
    OutOfMemory,
    /// A pixel or output buffer is smaller than the image dimensions require.
    BufferTooSmall,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::QualityTooLow => "image could not be quantized within the minimum quality",
            Self::ValueOutOfRange => "value out of range",
            Self::OutOfMemory => "out of memory",
            Self::BufferTooSmall => "buffer too small",
        })
    }
}

impl std::error::Error for Error {}
