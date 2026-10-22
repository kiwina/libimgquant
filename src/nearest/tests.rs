use super::exact::{ExactNearest, dist, embed};
use super::reference;
use super::vp::VpNearest;
use crate::pal::{FPixel, PalF, PalItem};

/// splitmix64, so the tests don't depend on the crate's own generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A premultiplied color; often opaque, sometimes fully transparent.
    fn color(&mut self) -> FPixel {
        let a = match self.below(4) {
            0 => 1.0,
            1 if self.below(4) == 0 => 0.0,
            _ => self.unit(),
        };
        FPixel {
            a,
            r: self.unit() * a,
            g: self.unit() * a,
            b: self.unit() * a,
        }
    }

    /// A color like the ones remapping sees: palette colors, small perturbations of them,
    /// points between two of them (near-ties), dithered values slightly outside 0..1,
    /// and arbitrary colors.
    fn query(&mut self, palette: &[FPixel]) -> FPixel {
        let p = palette[self.below(palette.len())];
        let q = palette[self.below(palette.len())];
        match self.below(6) {
            0 => p,
            1 => {
                let e = 1.0 / 256.0;
                let mut n = || (self.unit() - 0.5) * e;
                FPixel {
                    a: (p.a + n()).clamp(0.0, 1.0),
                    r: p.r + n(),
                    g: p.g + n(),
                    b: p.b + n(),
                }
            }
            2 => {
                let t = match self.below(3) {
                    0 => 0.5,
                    1 => 0.5 + (self.unit() - 0.5) * 1e-6,
                    _ => self.unit(),
                };
                let lerp = |x: f32, y: f32| x + (y - x) * t;
                FPixel {
                    a: lerp(p.a, q.a),
                    r: lerp(p.r, q.r),
                    g: lerp(p.g, q.g),
                    b: lerp(p.b, q.b),
                }
            }
            3 => FPixel {
                a: self.unit(),
                r: self.unit() * 1.1 - 0.05,
                g: self.unit() * 1.1 - 0.05,
                b: self.unit() * 1.1 - 0.05,
            },
            _ => self.color(),
        }
    }

    fn palette(&mut self, n: usize) -> Vec<FPixel> {
        let mut colors: Vec<FPixel> = (0..n).map(|_| self.color()).collect();
        // duplicates and near-duplicates
        for _ in 0..n / 10 {
            let (i, j) = (self.below(n), self.below(n));
            colors[i] = colors[j];
            if self.below(2) == 0 {
                colors[i].r += 1e-6;
            }
        }
        colors
    }
}

fn pal(colors: &[FPixel], subset: Option<&[FPixel]>) -> PalF {
    let item = |&color| PalItem {
        color,
        popularity: 1.0,
        fixed: false,
    };
    PalF {
        items: colors.iter().map(item).collect(),
        subset: subset.map(|s| s.iter().map(item).collect()),
    }
}

const SIZES: &[usize] = &[
    1, 2, 3, 5, 8, 13, 16, 17, 18, 24, 31, 32, 47, 64, 100, 128, 177, 200, 255, 256,
];

fn assert_compat_equal(map: &PalF, queries: &[FPixel], rng: &mut Rng) {
    let n = map.len();
    for fast in [false, true] {
        let old = reference::Nearest::new(map, fast);
        let new = VpNearest::new(map, fast);
        for min_opaque_val in [1.0, 0.7, 0.0] {
            let mut last = 0;
            for &px in queries {
                // the previous match, or any guess including out of range ones
                let guess = if rng.below(2) == 0 { last } else { rng.below(n + 3) };
                let a = old.search(px, guess, min_opaque_val);
                let b = new.search(px, guess, min_opaque_val);
                assert!(
                    a.0 == b.0 && a.1.to_bits() == b.1.to_bits(),
                    "n={n} fast={fast} min_opaque={min_opaque_val} guess={guess} px={px:?}: {a:?} != {b:?}"
                );
                last = a.0;
            }
        }
    }
}

#[test]
fn compat_matches_reference() {
    let mut rng = Rng(1);
    for round in 0..3 {
        for &n in SIZES {
            let colors = rng.palette(n);
            // median cut remembers a smaller palette; without one the first quarter is used
            let subset: Vec<FPixel> = (0..1 + rng.below(n)).map(|_| rng.color()).collect();
            let map = pal(&colors, (round != 0).then_some(&subset[..]));
            let queries: Vec<FPixel> = (0..1500).map(|_| rng.query(&colors)).collect();
            assert_compat_equal(&map, &queries, &mut rng);
        }
    }
}

#[test]
fn compat_matches_reference_on_odd_inputs() {
    let mut rng = Rng(2);
    let odd = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1e30,
        -1e30,
        f32::MAX,
        1e-40,
        -0.0,
    ];
    for &n in &[5, 40, 256] {
        let colors = rng.palette(n);
        let mut queries: Vec<FPixel> = (0..300).map(|_| rng.query(&colors)).collect();
        for &v in &odd {
            for ch in 0..4 {
                let mut px = rng.query(&colors);
                match ch {
                    0 => px.a = v,
                    1 => px.r = v,
                    2 => px.g = v,
                    _ => px.b = v,
                }
                queries.push(px);
            }
        }
        assert_compat_equal(&pal(&colors, None), &queries, &mut rng);
        // palettes with garbage colors take the scalar path (NaN makes 2.4.1's sort panic)
        for &v in &odd[3..] {
            let mut colors = colors.clone();
            colors[n / 2].g = v;
            assert_compat_equal(&pal(&colors, None), &queries, &mut rng);
        }
    }
}

fn assert_exact(colors: &[FPixel], queries: &[FPixel], rng: &mut Rng) {
    let n = colors.len();
    let exact = ExactNearest::new(colors);
    for min_opaque_val in [1.0, 0.7, 0.0] {
        let mut last = 0;
        for &px in queries {
            let iebug = px.a > min_opaque_val;
            let expected = exact.brute_force(&embed(px), iebug);
            let guess = if rng.below(2) == 0 { last } else { rng.below(n + 3) };
            let got = exact.search(px, guess, min_opaque_val);
            assert!(
                expected.0 == got.0 && expected.1.to_bits() == got.1.to_bits(),
                "n={n} min_opaque={min_opaque_val} guess={guess} px={px:?}: {got:?}, brute force {expected:?}"
            );
            last = got.0;
        }
    }
}

#[test]
fn improved_matches_brute_force() {
    let mut rng = Rng(3);
    for _ in 0..3 {
        for &n in SIZES {
            let colors = rng.palette(n);
            let queries: Vec<FPixel> = (0..3000).map(|_| rng.query(&colors)).collect();
            assert_exact(&colors, &queries, &mut rng);
        }
    }
}

#[test]
fn improved_matches_brute_force_on_odd_inputs() {
    let mut rng = Rng(4);
    for &n in &[5, 40, 256] {
        let colors = rng.palette(n);
        let mut queries: Vec<FPixel> = (0..300).map(|_| rng.query(&colors)).collect();
        for v in [f32::NAN, f32::INFINITY, -1e30, 1e4, f32::MAX, 1e-40] {
            queries.push(FPixel { r: v, ..colors[0] });
            queries.push(FPixel { a: v, ..colors[0] });
        }
        assert_exact(&colors, &queries, &mut rng);
        let mut garbage = colors.clone();
        garbage[n / 2].b = 1e30;
        assert_exact(&garbage, &queries, &mut rng);
    }
}

/// Many colors equally far from the query: the lowest index must win.
#[test]
fn improved_ties_go_to_the_lowest_index() {
    let gray = |v: f32| FPixel::splat(1.0).with_rgb(v);
    let mut colors: Vec<FPixel> = (0..40).map(|i| gray(i as f32 / 40.0)).collect();
    colors[30] = gray(0.25);
    colors[35] = gray(0.75);
    colors[37] = gray(0.25);
    let exact = ExactNearest::new(&colors);
    for guess in 0..42 {
        // 0.25 is at index 10 too
        assert_eq!(exact.search(gray(0.25), guess, 1.0).0, 10);
        assert_eq!(exact.search(gray(0.26), guess, 1.0).0, 10);
    }
}

#[test]
fn embedding_matches_diff() {
    let mut rng = Rng(5);
    for _ in 0..100_000 {
        let p = rng.color();
        let q = if rng.below(2) == 0 {
            rng.color()
        } else {
            rng.query(&[p])
        };
        let diff = f64::from(p.diff(&q));
        let d = dist(&embed(p), &embed(q));
        assert!((d - diff).abs() <= 1e-6 * diff + 1e-30, "{p:?} {q:?}: {d} vs {diff}");
    }
}

impl FPixel {
    fn with_rgb(self, v: f32) -> Self {
        Self {
            r: v * self.a,
            g: v * self.a,
            b: v * self.a,
            ..self
        }
    }
}
