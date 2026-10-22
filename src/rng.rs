//! Seeded random numbers for the initial dithering noise.
//!
//! libimagequant 2.4.1 seeded the process-global `rand()` for this, so its output depended
//! on the platform (and raced with other users of `rand()`). This is the additive lagged
//! Fibonacci generator `x[i] = x[i-3] + x[i-31]` (as used by GNU `random()`), owned by the
//! remapping call, so dithering is deterministic on every platform and thread-safe.

pub(crate) const RAND_MAX: i32 = 0x7fff_ffff;

pub(crate) struct DitherRng {
    state: [i32; 31],
    f: usize,
    r: usize,
}

impl DitherRng {
    pub fn new(seed: u32) -> Self {
        let mut state = [0i32; 31];
        let mut word = i64::from(if seed == 0 { 1 } else { seed as i32 });
        state[0] = word as i32;
        for s in &mut state[1..] {
            let hi = word / 127_773;
            let lo = word % 127_773;
            word = 16807 * lo - 2836 * hi;
            if word < 0 {
                word += 2_147_483_647;
            }
            *s = word as i32;
        }
        let mut rng = Self { state, f: 3, r: 0 };
        for _ in 0..310 {
            rng.next();
        }
        rng
    }

    pub fn next(&mut self) -> i32 {
        let val = (self.state[self.f] as u32).wrapping_add(self.state[self.r] as u32);
        self.state[self.f] = val as i32;
        self.f += 1;
        self.r += 1;
        if self.f >= self.state.len() {
            self.f = 0;
        } else if self.r >= self.state.len() {
            self.r = 0;
        }
        (val >> 1) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sequence_for_seed_1() {
        let mut rng = DitherRng::new(1);
        assert_eq!(
            [rng.next(), rng.next(), rng.next()],
            [1_804_289_383, 846_930_886, 1_681_692_777]
        );
    }
}
