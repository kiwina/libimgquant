//! The original nearest-color search, kept verbatim as the reference for equivalence tests
//! of the vectorized Compat search in `vp.rs`.

use std::cmp::Ordering;

use crate::pal::{FPixel, MAX_DIFF, PalF, cmin};

struct Head {
    /// Colors less than `radius` away from `vantage_point` have their best match in candidates.
    vantage_point: FPixel,
    radius: f32,
    candidates_color: Vec<FPixel>,
    candidates_index: Vec<u16>,
}

pub(crate) struct Nearest {
    colors: Vec<FPixel>,
    nearest_other_color_dist: Vec<f32>,
    heads: Vec<Head>,
}

fn distance_from_nearest_other_color(colors: &[FPixel], i: usize) -> f32 {
    let mut second_best = MAX_DIFF as f32;
    for (j, c) in colors.iter().enumerate() {
        if i == j {
            continue;
        }
        let diff = colors[i].diff(c);
        if diff <= second_best {
            second_best = diff;
        }
    }
    second_best
}

fn build_head(
    px: FPixel,
    colors: &[FPixel],
    num_candidates: usize,
    error_margin: f32,
    skip_index: &mut [bool],
    skipped: &mut usize,
) -> Head {
    // colors in skip_index have been eliminated already in previous heads
    let mut sorted: Vec<(f32, usize)> = colors
        .iter()
        .enumerate()
        .filter(|&(i, _)| !skip_index[i])
        .map(|(i, c)| (px.diff(c), i))
        .collect();
    // stable sort, closest first; ties keep palette order
    sorted.sort_by(|a, b| {
        if a.0 > b.0 {
            Ordering::Greater
        } else if a.0 < b.0 {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    });

    let num_candidates = cmin(sorted.len(), num_candidates);
    let candidates = &sorted[..num_candidates];
    let mut head = Head {
        vantage_point: px,
        radius: 0.0,
        candidates_color: candidates.iter().map(|&(_, i)| colors[i]).collect(),
        candidates_index: candidates.iter().map(|&(_, i)| i as u16).collect(),
    };
    let Some(&(_, farthest)) = candidates.last() else {
        return head;
    };

    // if all colors within this radius are included in candidates, then there cannot be any other better match
    // farther away from the vantage point than half of the radius. Due to alpha channel must assume pessimistic radius.
    head.radius = px.min_diff(&colors[farthest]) / 4.0; // /4 = half of radius, but radius is squared

    for &(radius, i) in candidates {
        // divide again as that's matching certain subset within radius-limited subset
        if radius < head.radius / 4.0 - error_margin {
            skip_index[i] = true;
            *skipped += 1;
        }
    }
    head
}

impl Nearest {
    pub fn new(map: &PalF, fast: bool) -> Self {
        let colors: Vec<FPixel> = map.items.iter().map(|p| p.color).collect();
        let n = colors.len();
        assert!(n > 0, "palette must not be empty");

        let subset: Vec<FPixel> = match &map.subset {
            Some(s) => s.iter().map(|p| p.color).collect(),
            None => colors[..n.div_ceil(4)].to_vec(),
        };
        let num_vantage_points = if n > 16 {
            cmin(n / if fast { 4 } else { 3 }, subset.len())
        } else {
            0
        };

        let nearest_other_color_dist = (0..n)
            .map(|i| distance_from_nearest_other_color(&colors, i) / 4.0)
            .collect(); // half of squared distance

        let mut skip_index = vec![false; n];
        let mut skipped = 0;
        // floats and colordifference calculations are not perfect
        let error_margin = if fast { 0.0 } else { 8.0f32 / 256.0 / 256.0 };

        let mut heads = Vec::with_capacity(num_vantage_points + 1);
        for (h, &vantage_point) in subset.iter().enumerate().take(num_vantage_points) {
            let num_candidates = 1 + (n - skipped) / ((1 + num_vantage_points - h) / 2);
            let head = build_head(
                vantage_point,
                &colors,
                num_candidates,
                error_margin,
                &mut skip_index,
                &mut skipped,
            );
            if head.candidates_index.is_empty() {
                break;
            }
            heads.push(head);
        }

        // assumption that there is no better color within radius of vantage point color
        // holds true only for colors within convex hull formed by palette colors.
        // The fallback must contain all colors, since there are too many edge cases to cover.
        if !fast {
            skip_index.fill(false);
        }
        let mut fallback = build_head(
            FPixel::default(),
            &colors,
            n,
            error_margin,
            &mut skip_index,
            &mut skipped,
        );
        if fallback.candidates_index.is_empty() {
            // the original read past the end here; search everything instead
            skip_index.fill(false);
            fallback = build_head(
                FPixel::default(),
                &colors,
                n,
                error_margin,
                &mut skip_index,
                &mut skipped,
            );
        }
        fallback.radius = MAX_DIFF as f32;
        heads.push(fallback);

        Self {
            colors,
            nearest_other_color_dist,
            heads,
        }
    }

    /// Returns the index of the closest palette color, and its difference.
    #[inline]
    pub fn search(&self, px: FPixel, likely_colormap_index: usize, min_opaque_val: f32) -> (usize, f32) {
        let iebug = px.a > min_opaque_val;

        let likely = if likely_colormap_index < self.colors.len() {
            likely_colormap_index
        } else {
            0
        };
        let guess_diff = self.colors[likely].diff(&px);
        if guess_diff < self.nearest_other_color_dist[likely] {
            return (likely, guess_diff);
        }

        let last = self.heads.len() - 1;
        for (i, head) in self.heads.iter().enumerate() {
            // the last head is always selected
            if i != last && px.diff(&head.vantage_point) > head.radius {
                continue;
            }
            let mut ind = 0;
            let mut dist = px.diff(&head.candidates_color[0]);
            // penalty for making holes in IE
            if iebug && head.candidates_color[0].a < 1.0 {
                dist += 1.0 / 1024.0;
            }
            for (j, c) in head.candidates_color.iter().enumerate().skip(1) {
                let mut newdist = px.diff(c);
                if iebug && c.a < 1.0 {
                    newdist += 1.0 / 1024.0;
                }
                if newdist < dist {
                    dist = newdist;
                    ind = j;
                }
            }
            return (usize::from(head.candidates_index[ind]), dist);
        }
        unreachable!()
    }
}
