//! Box blur and min/max filters used for contrast maps.
//!
//! Every output row depends only on input rows, so all filters run row-parallel with the
//! `threads` feature. The arithmetic is integer and per row, so the result is byte-identical
//! to a sequential pass for any thread count.

#[cfg(feature = "threads")]
use rayon::prelude::*;

/// Rows handed to one rayon task at least, so that tiny rows don't cost a task each.
#[cfg(feature = "threads")]
fn min_rows_per_task(width: usize) -> usize {
    (4096 / width.max(1)).max(1)
}

/// Calls `f(state, row_index, row)` for every row of `buf` (rows of `width` items), in
/// parallel with the `threads` feature. `init` creates per-task scratch state.
pub(crate) fn for_each_row<T: Send, S>(
    buf: &mut [T],
    width: usize,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, usize, &mut [T]) + Sync + Send,
) {
    #[cfg(feature = "threads")]
    buf.par_chunks_mut(width)
        .with_min_len(min_rows_per_task(width))
        .enumerate()
        .for_each_init(init, |state, (j, row)| f(state, j, row));
    #[cfg(not(feature = "threads"))]
    {
        let mut state = init();
        for (j, row) in buf.chunks_mut(width).enumerate() {
            f(&mut state, j, row);
        }
    }
}

/// Horizontal box blur of one row: `out[i]` is the sum of `row[i-size+1..=i+size]`, with
/// indices clamped to the row, divided by `2*size`.
///
/// This is the running sum of 2.4.1's `transposing_1d_blur` for one row: the sum starts as
/// `row[0]*size + row[0..size]` and each step removes `row[max(i-size, 0)]` and adds
/// `row[min(i+size, width-1)]`.
fn blur_row(row: &[u8], out: &mut [u8], size: usize) {
    let width = row.len();
    let size_u32 = size as u32;
    let div = size_u32 * 2;
    // the sum never exceeds 2*size*255, so it can't overflow or go negative
    let mut sum = u32::from(row[0]) * size_u32;
    for &px in &row[..size] {
        sum += u32::from(px);
    }

    // blur with left side outside line
    for i in 0..size {
        sum = sum - u32::from(row[0]) + u32::from(row[i + size]);
        out[i] = (sum / div) as u8;
    }

    for i in size..width - size {
        sum = sum - u32::from(row[i - size]) + u32::from(row[i + size]);
        out[i] = (sum / div) as u8;
    }

    // blur with right side outside line
    for i in width - size..width {
        sum = sum - u32::from(row[i - size]) + u32::from(row[width - 1]);
        out[i] = (sum / div) as u8;
    }
}

fn filter3(src: &[u8], dst: &mut [u8], width: usize, height: usize, pick: impl Fn(u8, u8) -> u8 + Sync + Send) {
    debug_assert_eq!(dst.len(), width * height);
    for_each_row(
        dst,
        width,
        || (),
        |(), j, out| {
            let row = &src[j * width..(j + 1) * width];
            let prevrow = &src[j.saturating_sub(1) * width..][..width];
            let nextrow = &src[(height - 1).min(j + 1) * width..][..width];

            let mut curr = row[0];
            let mut next = row[0];
            for i in 0..width - 1 {
                let prev = curr;
                curr = next;
                next = row[i + 1];

                let t1 = pick(prev, next);
                let t2 = pick(nextrow[i], prevrow[i]);
                out[i] = pick(curr, pick(t1, t2));
            }
            let t1 = pick(curr, next);
            let t2 = pick(nextrow[width - 1], prevrow[width - 1]);
            out[width - 1] = pick(t1, t2);
        },
    );
}

/// Picks maximum of neighboring pixels (blur + lighten).
pub(crate) fn max3(src: &[u8], dst: &mut [u8], width: usize, height: usize) {
    filter3(src, dst, width, height, std::cmp::max);
}

/// Picks minimum of neighboring pixels (blur + darken).
pub(crate) fn min3(src: &[u8], dst: &mut [u8], width: usize, height: usize) {
    filter3(src, dst, width, height, std::cmp::min);
}

/// Box-blurs `buf` in place using `tmp` as scratch space. `size` controls the radius.
///
/// 2.4.1 blurred rows into a transposed buffer and blurred that again (two calls of
/// `transposing_1d_blur`). The same separable filter is computed here without transposing,
/// so both passes are row-parallel: the rows are blurred horizontally into `tmp`, then each
/// output row sums the vertical window of `tmp` rows `[j-size+1, j+size]` (clamped) and
/// divides by `2*size`, exactly what the running sum over the transposed column computed.
/// `tmp` holds the horizontal pass afterwards, not 2.4.1's transposed buffer; callers
/// overwrite it.
pub(crate) fn blur(buf: &mut [u8], tmp: &mut [u8], width: usize, height: usize, size: usize) {
    debug_assert!(size > 0);
    if width < 2 * size + 1 || height < 2 * size + 1 {
        return;
    }
    let src: &[u8] = buf;
    for_each_row(
        tmp,
        width,
        || (),
        |(), j, out| blur_row(&src[j * width..(j + 1) * width], out, size),
    );

    let tmp: &[u8] = tmp;
    let div = (size * 2) as u32;
    for_each_row(
        buf,
        width,
        || vec![0u32; width],
        |acc, j, out| {
            acc.fill(0);
            // all 2*size rows of the window, top clamped to row 0 and bottom to the last row;
            // integer sums, so the order of additions doesn't matter
            for k in (j + 1).saturating_sub(size)..=(j + size).min(height - 1) {
                for (a, &v) in acc.iter_mut().zip(&tmp[k * width..(k + 1) * width]) {
                    *a += u32::from(v);
                }
            }
            let top_clamped = (size - 1).saturating_sub(j) as u32;
            let bottom_clamped = (j + size).saturating_sub(height - 1) as u32;
            let (first, last) = (&tmp[..width], &tmp[(height - 1) * width..]);
            for (((o, &a), &f), &l) in out.iter_mut().zip(acc.iter()).zip(first).zip(last) {
                let sum = a + u32::from(f) * top_clamped + u32::from(l) * bottom_clamped;
                *o = (sum / div) as u8;
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2.4.1's transposing blur, kept as the reference.
    fn transposing_1d_blur(src: &[u8], dst: &mut [u8], width: usize, height: usize, size: usize) {
        let size_u32 = size as u32;
        for j in 0..height {
            let row = &src[j * width..(j + 1) * width];
            let mut sum = u32::from(row[0]) * size_u32;
            for &px in &row[..size] {
                sum = sum.wrapping_add(u32::from(px));
            }
            for i in 0..size {
                sum = sum
                    .wrapping_sub(u32::from(row[0]))
                    .wrapping_add(u32::from(row[i + size]));
                dst[i * height + j] = (sum / (size_u32 * 2)) as u8;
            }
            for i in size..width - size {
                sum = sum
                    .wrapping_sub(u32::from(row[i - size]))
                    .wrapping_add(u32::from(row[i + size]));
                dst[i * height + j] = (sum / (size_u32 * 2)) as u8;
            }
            for i in width - size..width {
                sum = sum
                    .wrapping_sub(u32::from(row[i - size]))
                    .wrapping_add(u32::from(row[width - 1]));
                dst[i * height + j] = (sum / (size_u32 * 2)) as u8;
            }
        }
    }

    fn reference_blur(buf: &mut [u8], tmp: &mut [u8], width: usize, height: usize, size: usize) {
        if width < 2 * size + 1 || height < 2 * size + 1 {
            return;
        }
        transposing_1d_blur(buf, tmp, width, height, size);
        transposing_1d_blur(tmp, buf, height, width, size);
    }

    /// 2.4.1's sequential min/max filter.
    fn reference_filter3(src: &[u8], dst: &mut [u8], width: usize, height: usize, pick: fn(u8, u8) -> u8) {
        let mut out = dst.iter_mut();
        for j in 0..height {
            let row = &src[j * width..(j + 1) * width];
            let prevrow = &src[j.saturating_sub(1) * width..][..width];
            let nextrow = &src[(height - 1).min(j + 1) * width..][..width];
            let mut curr = row[0];
            let mut next = row[0];
            for i in 0..width - 1 {
                let prev = curr;
                curr = next;
                next = row[i + 1];
                let t1 = pick(prev, next);
                let t2 = pick(nextrow[i], prevrow[i]);
                *out.next().unwrap() = pick(curr, pick(t1, t2));
            }
            let t1 = pick(curr, next);
            let t2 = pick(nextrow[width - 1], prevrow[width - 1]);
            *out.next().unwrap() = pick(t1, t2);
        }
    }

    fn random_map(width: usize, height: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..width * height)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                // mix of noise, flat areas and extremes
                match (i / 7 + seed as usize) % 4 {
                    0 => 255,
                    1 => (x >> 24) as u8,
                    2 => 0,
                    _ => (x >> 28) as u8 * 17,
                }
            })
            .collect()
    }

    #[test]
    fn blur_matches_transposing_blur() {
        for (seed, (w, h)) in [
            (1, 1),
            (2, 3),
            (6, 6),
            (7, 7),
            (7, 9),
            (8, 7),
            (9, 30),
            (31, 8),
            (64, 64),
            (65, 17),
            (100, 7),
            (7, 100),
            (300, 211),
        ]
        .into_iter()
        .enumerate()
        {
            for size in 1..=4 {
                let input = random_map(w, h, seed as u32 * 31 + size as u32);
                let (mut a, mut b) = (input.clone(), input);
                let (mut ta, mut tb) = (vec![0; w * h], vec![0; w * h]);
                reference_blur(&mut a, &mut ta, w, h, size);
                blur(&mut b, &mut tb, w, h, size);
                assert_eq!(a, b, "{w}x{h} size {size}");
            }
        }
    }

    #[test]
    fn min_max_match_sequential_filter() {
        for (seed, (w, h)) in [(1, 1), (1, 5), (5, 1), (2, 2), (3, 7), (64, 33), (257, 129)]
            .into_iter()
            .enumerate()
        {
            let input = random_map(w, h, seed as u32);
            let (mut a, mut b) = (vec![0; w * h], vec![0; w * h]);
            reference_filter3(&input, &mut a, w, h, std::cmp::max);
            max3(&input, &mut b, w, h);
            assert_eq!(a, b);
            reference_filter3(&input, &mut a, w, h, std::cmp::min);
            min3(&input, &mut b, w, h);
            assert_eq!(a, b);
        }
    }
}
