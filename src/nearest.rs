//! Nearest palette color search.
//!
//! - `Profile::Compat`: the vantage-point search of libimagequant 2.4.1 (`vp.rs`),
//!   vectorized, with results identical bit for bit to the original (kept in `reference.rs`
//!   for tests).
//! - `Profile::Improved`: an exact search (`exact.rs`), which returns the true nearest color.

mod exact;
#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
mod vp;

use crate::attr::Profile;
use crate::pal::{FPixel, PalF};

// Only one searcher exists at a time, so the size difference doesn't matter.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Nearest {
    Compat(vp::VpNearest),
    Improved(exact::ExactNearest),
}

impl Nearest {
    /// Prepares a search over the palette. `fast` trades accuracy for speed in the Compat
    /// profile; the Improved search is always exact.
    pub fn new(map: &PalF, fast: bool, profile: Profile) -> Self {
        match profile {
            Profile::Compat => Self::Compat(vp::VpNearest::new(map, fast)),
            Profile::Improved => {
                let colors: Vec<FPixel> = map.items.iter().map(|p| p.color).collect();
                Self::Improved(exact::ExactNearest::new(&colors))
            }
        }
    }

    /// Returns the index of the closest palette color, and its difference.
    ///
    /// `likely_colormap_index` is a guess (out of range is allowed). Pixels more opaque than
    /// `min_opaque_val` get a penalty for translucent colors.
    #[inline]
    pub fn search(&self, px: FPixel, likely_colormap_index: usize, min_opaque_val: f32) -> (usize, f32) {
        match self {
            Self::Compat(n) => n.search(px, likely_colormap_index, min_opaque_val),
            Self::Improved(n) => n.search(px, likely_colormap_index, min_opaque_val),
        }
    }
}
