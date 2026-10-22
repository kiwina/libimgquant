//! Palette order for smaller PNG files.
//!
//! PNG filters (Sub, Up, Paeth, …) predict each byte from its neighbors, so they shrink
//! an indexed image only when neighboring pixels have numerically close indices. Colors
//! that often touch in the image are therefore given adjacent indices: the palette order
//! approximately minimizes the linear arrangement cost `Σ m(a,b)·|pos(a) − pos(b)|`, where
//! `m(a,b)` counts how often `a` and `b` are horizontal or vertical neighbors.
//!
//! The order is built with the modified Zeng method and refined by moving single colors
//! to their best position (Zeng, Li & Lei, "An efficient color re-indexing scheme for
//! palette-based compression", ICIP 2000; Pinho & Neves, "A survey on palette reordering
//! methods for improving the compression of color-indexed images", IEEE TIP 13(11), 2004,
//! and "On the relation between Memon's and the modified Zeng's palette reordering
//! methods", Image and Vision Computing 24, 2006).
//!
//! Only indices change, never pixels. With an unfiltered PNG encoding the order makes no
//! difference; encoders that try filters (or keep the smaller of a filtered and an
//! unfiltered encoding) produce smaller files.

use std::collections::VecDeque;

use crate::RGBA;

/// Upper bound on refinement sweeps; they usually stop much earlier.
const MAX_SWEEPS: usize = 2;

/// How often each pair of different indices is adjacent, as an `n × n` symmetric matrix.
fn cooccurrence(rows: &[&mut [u8]], n: usize) -> Vec<u64> {
    // u32 counts per band (a band has far fewer than 2^31 pixels), summed into u64
    let count = |range: std::ops::Range<usize>| {
        let mut m = vec![0u32; n * n];
        for y in range {
            let row = &rows[y];
            for x in 1..row.len() {
                let (v, l) = (usize::from(row[x]), usize::from(row[x - 1]));
                if v != l {
                    m[v * n + l] += 1;
                    m[l * n + v] += 1;
                }
            }
            if y > 0 {
                for (&v, &u) in row.iter().zip(rows[y - 1].iter()) {
                    let (v, u) = (usize::from(v), usize::from(u));
                    if v != u {
                        m[v * n + u] += 1;
                        m[u * n + v] += 1;
                    }
                }
            }
        }
        m
    };
    // few large bands, so that only a few matrices exist at once; integer sums make the
    // result independent of how the bands are processed
    let width = rows.first().map_or(1, |r| r.len().max(1));
    let band = (1 << 20) / width + 1;
    let bands: Vec<std::ops::Range<usize>> = (0..rows.len())
        .step_by(band)
        .map(|y| y..(y + band).min(rows.len()))
        .collect();
    let mut total = vec![0u64; n * n];
    let add = |total: &mut Vec<u64>, m: Vec<u32>| total.iter_mut().zip(m).for_each(|(t, v)| *t += u64::from(v));
    #[cfg(feature = "threads")]
    {
        use rayon::prelude::*;
        for m in bands.into_par_iter().map(count).collect::<Vec<_>>() {
            add(&mut total, m);
        }
    }
    #[cfg(not(feature = "threads"))]
    for range in bands {
        add(&mut total, count(range));
    }
    total
}

/// Modified Zeng: start from the most connected color and its strongest partner, then
/// repeatedly take the remaining color most connected to the placed ones and add it at
/// whichever end of the list lowers the arrangement cost.
fn modified_zeng(m: &[u64], n: usize) -> Vec<usize> {
    let w = |a: usize, b: usize| m[a * n + b];
    let row_sum = |a: usize| (0..n).map(|b| w(a, b)).sum::<u64>();
    let c1 = (0..n)
        .max_by_key(|&a| (row_sum(a), std::cmp::Reverse(a)))
        .expect("n > 2");
    let c2 = (0..n)
        .filter(|&b| b != c1)
        .max_by_key(|&b| (w(c1, b), std::cmp::Reverse(b)))
        .expect("n > 2");

    let mut list = VecDeque::from([c1, c2]);
    let mut placed = vec![false; n];
    placed[c1] = true;
    placed[c2] = true;
    let mut sums: Vec<u64> = (0..n).map(|u| w(u, c1) + w(u, c2)).collect();
    for _ in 2..n {
        let u = (0..n)
            .filter(|&u| !placed[u])
            .max_by_key(|&u| (sums[u], std::cmp::Reverse(u)))
            .expect("colors left");
        // cost(front) − cost(back) = −Σ_j m(u, list[j])·(len − 1 − 2j)
        let len = list.len() as i64;
        let delta: i64 = list
            .iter()
            .enumerate()
            .map(|(j, &s)| (len - 1 - 2 * j as i64) * w(u, s) as i64)
            .sum();
        if delta > 0 {
            list.push_front(u);
        } else {
            list.push_back(u);
        }
        placed[u] = true;
        for (s, sum) in sums.iter_mut().enumerate() {
            *sum += w(s, u);
        }
    }
    list.into()
}

/// `cut[k]` = total weight of pairs with one color before position `k` and one at or after it.
fn cut_profile(m: &[u64], n: usize, order: &[usize]) -> Vec<i64> {
    let mut cut = vec![0i64; n + 1];
    for k in 0..n {
        let c = order[k];
        let before: i64 = order[..k].iter().map(|&o| m[c * n + o] as i64).sum();
        let after: i64 = order[k + 1..].iter().map(|&o| m[c * n + o] as i64).sum();
        cut[k + 1] = cut[k] + after - before;
    }
    cut
}

/// Moves the color at position `p` to position `q`, updating `cut` in O(n).
///
/// Only boundaries between the two positions change. With `W` the color's total weight
/// and `L(k)` its weight to the colors before position `k` (in the old order): moving it
/// left, boundary `k` in `q+1..=p` now has the color on its left side, so
/// `cut'(k) = cut(k-1) + W - 2·L(k-1)`; moving it right, boundary `k` in `p..q` loses it
/// from the left side, so `cut'(k) = cut(k+1) - W + 2·L(k+1)`.
fn move_color(m: &[u64], n: usize, order: &mut Vec<usize>, cut: &mut [i64], p: usize, q: usize) {
    let c = order[p];
    let mut prefix = vec![0i64; n + 1];
    for (i, &o) in order.iter().enumerate() {
        prefix[i + 1] = prefix[i] + m[c * n + o] as i64;
    }
    let total = prefix[n];
    let old = cut.to_vec();
    if q < p {
        for k in q + 1..=p {
            cut[k] = old[k - 1] + total - 2 * prefix[k - 1];
        }
    } else {
        for k in p..q {
            cut[k + 1] = old[k + 2] - total + 2 * prefix[k + 2];
        }
    }
    order.remove(p);
    order.insert(q, c);
}

/// Moves single colors to the position that lowers the arrangement cost most, until no
/// move helps. For a color at position `p`, inserting it at slot `q` of the other colors
/// costs (up to a constant) the weight of pairs straddling slot `q`, which all get one
/// position farther apart, plus the color's own distances to every other color.
fn refine(m: &[u64], n: usize, order: &mut Vec<usize>) {
    let mut ws = vec![0i64; n];
    for _ in 0..MAX_SWEEPS {
        let mut moved = false;
        let mut cut = cut_profile(m, n, order);
        for c in 0..n {
            let p = order.iter().position(|&o| o == c).expect("color in order");
            // weights to the other colors, in their order without `c`
            for (i, slot) in ws.iter_mut().take(n - 1).enumerate() {
                let o = order[if i < p { i } else { i + 1 }];
                *slot = m[c * n + o] as i64;
            }
            let total_w: i64 = ws[..n - 1].iter().sum();
            let total_wi: i64 = ws[..n - 1].iter().enumerate().map(|(i, &x)| x * i as i64).sum();
            let (mut left_w, mut left_wi) = (0i64, 0i64);
            let mut best = (i64::MAX, p);
            let mut current = i64::MAX;
            for q in 0..n {
                let straddle = if q <= p {
                    cut[q] - left_w
                } else {
                    cut[q + 1] - (total_w - left_w)
                };
                let q64 = q as i64;
                let own = q64 * left_w - left_wi + (total_wi - left_wi) + (1 - q64) * (total_w - left_w);
                let cost = straddle + own;
                if q == p {
                    current = cost;
                }
                if cost < best.0 {
                    best = (cost, q);
                }
                if q < n - 1 {
                    left_w += ws[q];
                    left_wi += ws[q] * q64;
                }
            }
            if best.0 < current && best.1 != p {
                move_color(m, n, order, &mut cut, p, best.1);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// With `by_neighbors`, orders the palette for compression (see the module docs). Then
/// moves translucent entries first, keeping their order, so the PNG tRNS chunk can stop
/// after the last of them (alpha can change class when colors are rounded after the first
/// sort). Rewrites the indices in `rows` to match.
pub(crate) fn reorder_palette(palette: &mut Vec<RGBA>, rows: &mut [&mut [u8]], by_neighbors: bool) {
    let n = palette.len();
    let mut order: Vec<usize> = (0..n).collect();
    if by_neighbors && n > 2 {
        let m = cooccurrence(rows, n);
        order = modified_zeng(&m, n);
        refine(&m, n, &mut order);
    }
    let order: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| palette[i].a != 255)
        .chain(order.iter().copied().filter(|&i| palette[i].a == 255))
        .collect();
    if order.iter().enumerate().all(|(new, &old)| new == old) {
        return;
    }
    let mut lut = [0u8; 256];
    for (new, &old) in order.iter().enumerate() {
        lut[old] = new as u8;
    }
    *palette = order.iter().map(|&i| palette[i]).collect();
    let rewrite = |row: &mut &mut [u8]| row.iter_mut().for_each(|p| *p = lut[usize::from(*p)]);
    #[cfg(feature = "threads")]
    {
        use rayon::prelude::*;
        rows.par_iter_mut().for_each(rewrite);
    }
    #[cfg(not(feature = "threads"))]
    rows.iter_mut().for_each(rewrite);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrangement_cost(m: &[u64], n: usize, order: &[usize]) -> i64 {
        let mut pos = vec![0i64; n];
        for (i, &c) in order.iter().enumerate() {
            pos[c] = i as i64;
        }
        let mut cost = 0;
        for a in 0..n {
            for b in a + 1..n {
                cost += m[a * n + b] as i64 * (pos[a] - pos[b]).abs();
            }
        }
        cost
    }

    fn random_matrix(n: usize, seed: u64) -> Vec<u64> {
        let mut s = seed;
        let mut m = vec![0u64; n * n];
        for a in 0..n {
            for b in a + 1..n {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let v = if (s >> 60) < 5 { (s >> 40) % 1000 } else { 0 };
                m[a * n + b] = v;
                m[b * n + a] = v;
            }
        }
        m
    }

    #[test]
    fn refinement_never_increases_cost_and_keeps_a_permutation() {
        for seed in 0..30 {
            let n = 3 + (seed as usize * 11) % 60;
            let m = random_matrix(n, seed);
            let mut order = modified_zeng(&m, n);
            let before = arrangement_cost(&m, n, &order);
            refine(&m, n, &mut order);
            assert!(arrangement_cost(&m, n, &order) <= before, "seed {seed}");
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>());
        }
    }

    /// The cost formula used for single moves must match a full recomputation.
    #[test]
    fn single_move_costs_are_exact() {
        let n = 9;
        let m = random_matrix(n, 99);
        let order: Vec<usize> = (0..n).rev().collect();
        let mut refined = order.clone();
        refine(&m, n, &mut refined);
        // brute force: no single move from the refined order improves the cost
        let cost = arrangement_cost(&m, n, &refined);
        for p in 0..n {
            for q in 0..n {
                let mut o = refined.clone();
                let c = o.remove(p);
                o.insert(q, c);
                assert!(arrangement_cost(&m, n, &o) >= cost, "move {p} -> {q} improves");
            }
        }
    }

    #[test]
    fn incremental_cut_matches_recomputation() {
        let n = 23;
        let m = random_matrix(n, 5);
        let mut order: Vec<usize> = (0..n).collect();
        let mut cut = cut_profile(&m, n, &order);
        for (p, q) in [
            (0, 22),
            (22, 0),
            (5, 6),
            (6, 5),
            (10, 3),
            (3, 17),
            (11, 11),
            (21, 2),
            (1, 20),
        ] {
            move_color(&m, n, &mut order, &mut cut, p, q);
            assert_eq!(cut, cut_profile(&m, n, &order), "move {p} -> {q}");
        }
    }

    #[test]
    fn pixels_are_unchanged_and_translucent_colors_come_first() {
        let mut palette: Vec<RGBA> = (0..20u8)
            .map(|i| RGBA::new(i * 10, 0, 255 - i * 10, if i % 6 == 5 { 128 } else { 255 }))
            .collect();
        let (w, h) = (31, 17);
        let mut data: Vec<u8> = (0..w * h).map(|i| ((i / 3 + i / w) % 20) as u8).collect();
        let original: Vec<RGBA> = data.iter().map(|&i| palette[usize::from(i)]).collect();
        let mut rows: Vec<&mut [u8]> = data.chunks_mut(w).collect();
        reorder_palette(&mut palette, &mut rows, true);
        let after: Vec<RGBA> = data.iter().map(|&i| palette[usize::from(i)]).collect();
        assert_eq!(original, after);
        let first_opaque = palette.iter().position(|c| c.a == 255).unwrap();
        assert!(palette[first_opaque..].iter().all(|c| c.a == 255));
        assert_eq!(first_opaque, 3);
    }
}
