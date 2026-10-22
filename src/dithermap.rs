//! Dither map: where Floyd-Steinberg dithering is applied.

use crate::blur::for_each_row;
use crate::image::Image;

/// Builds a map of neighbor pixels mapped to the same palette entry.
///
/// For efficiency/simplicity it mainly looks for same consecutive pixels horizontally
/// and peeks 1 pixel above/below. Full 2d algorithm doesn't improve it significantly.
/// Correct flood fill doesn't have visually good properties.
///
/// Each row of the map depends only on its own edge values and the remapped rows above,
/// at and below it, so rows are processed in parallel with identical results.
pub(crate) fn update_dither_map(output: &[&mut [u8]], image: &mut Image<'_>) {
    let width = image.width;
    let Some(mut edges) = image.edges.take() else {
        return;
    };
    let output = &output[..image.height];

    for_each_row(
        &mut edges,
        width,
        || (),
        |(), row, edges| update_row(output, row, edges),
    );
    image.dither_map = Some(edges);
}

/// Scales down the edge values of row `row` of the image for runs of pixels that were
/// remapped to the same palette entry, more the fewer neighbors share it.
fn update_row(output: &[&mut [u8]], row: usize, edges: &mut [u8]) {
    let width = edges.len();
    let mut lastpixel = output[row][0];
    let mut lastcol = 0;

    for col in 1..width {
        let px = output[row][col];
        if px != lastpixel || col == width - 1 {
            let mut neighbor_count = 2.5f32 + col as f32 - lastcol as f32;

            let above = row.checked_sub(1).map(|r| &output[r][lastcol..col]);
            let below = output.get(row + 1).map(|r| &r[lastcol..col]);
            for neighbors in [above, below].into_iter().flatten() {
                for &p in neighbors {
                    if p == lastpixel {
                        neighbor_count += 1.0;
                    }
                }
            }

            while lastcol <= col {
                let mut e = f32::from(edges[lastcol]) / 255.0;
                e *= 1.0 - 2.5 / neighbor_count;
                edges[lastcol] = (e * 255.0) as u8;
                lastcol += 1;
            }
            lastpixel = px;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequential 2.4.1 loop, kept as the reference.
    fn reference(output: &[&mut [u8]], edges: &mut [u8], width: usize, height: usize) {
        for row in 0..height {
            let mut lastpixel = output[row][0];
            let mut lastcol = 0;
            for col in 1..width {
                let px = output[row][col];
                if px != lastpixel || col == width - 1 {
                    let mut neighbor_count = 2.5f32 + col as f32 - lastcol as f32;
                    let above = row.checked_sub(1).map(|r| &output[r][lastcol..col]);
                    let below = output.get(row + 1).map(|r| &r[lastcol..col]);
                    for neighbors in [above, below].into_iter().flatten() {
                        for &p in neighbors {
                            if p == lastpixel {
                                neighbor_count += 1.0;
                            }
                        }
                    }
                    while lastcol <= col {
                        let mut e = f32::from(edges[row * width + lastcol]) / 255.0;
                        e *= 1.0 - 2.5 / neighbor_count;
                        edges[row * width + lastcol] = (e * 255.0) as u8;
                        lastcol += 1;
                    }
                    lastpixel = px;
                }
            }
        }
    }

    #[test]
    fn matches_sequential_loop() {
        let attr = crate::new();
        for (w, h) in [(1, 1), (1, 9), (9, 1), (2, 2), (13, 7), (200, 150)] {
            let mut x = (w * 31 + h) as u32 | 1;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            };
            // runs of equal indices of random lengths, so neighbors partly agree
            let mut out = vec![0u8; w * h];
            let mut v = 0;
            for o in &mut out {
                if next() % 5 == 0 {
                    v = (next() % 4) as u8;
                }
                *o = v;
            }
            let edges: Vec<u8> = (0..w * h).map(|_| next() as u8).collect();

            let mut rows: Vec<&mut [u8]> = out.chunks_mut(w).collect();
            let mut expected = edges.clone();
            reference(&rows, &mut expected, w, h);

            let pixels = vec![crate::RGBA::new(0, 0, 0, 255); w * h];
            let mut image = attr.new_image(pixels, w, h, 0.0).unwrap();
            image.edges = Some(edges.into_boxed_slice());
            update_dither_map(&rows, &mut image);
            assert_eq!(image.dither_map.as_deref(), Some(&expected[..]), "{w}x{h}");
            rows.clear();
        }
    }
}
