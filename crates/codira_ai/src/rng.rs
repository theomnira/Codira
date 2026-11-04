//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`Rng`]: a small deterministic pseudo-random generator
//!   (xoshiro256**, seeded through `SplitMix64`) for initialisers, synthetic
//!   data and property tests.
//!
//! Determinism is the point: the same seed gives the same stream on every
//! platform, because the generator uses only 64-bit integer arithmetic and
//! the float conversions are exact (53 high bits / 2^53). It is not a
//! cryptographic generator and must never be used as one.

/// xoshiro256** (Blackman and Vigna, 2018).
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// A generator whose whole stream is determined by `seed`.
    pub fn new(seed: u64) -> Self {
        let mut st = seed;
        let s = [
            splitmix64(&mut st),
            splitmix64(&mut st),
            splitmix64(&mut st),
            splitmix64(&mut st),
        ];
        Rng { s }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, 1)`, with 53 bits of resolution.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[lo, hi)`.
    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// Standard normal, by the Box-Muller transform.
    pub fn normal(&mut self) -> f64 {
        // 1 - u is in (0, 1], so the logarithm is finite.
        let u1 = 1.0 - self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Uniform integer in `0..n` (`n > 0`), by rejection so it is unbiased.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "Rng::below(0)");
        let n = n as u64;
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return (x % n) as usize;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn uniform_and_normal_moments_are_plausible() {
        let mut r = Rng::new(1);
        let n = 20_000;
        let (mut su, mut sn, mut sn2) = (0.0, 0.0, 0.0);
        for _ in 0..n {
            let u = r.next_f64();
            assert!((0.0..1.0).contains(&u));
            su += u;
            let z = r.normal();
            sn += z;
            sn2 += z * z;
        }
        let n = f64::from(n);
        assert!((su / n - 0.5).abs() < 0.01);
        assert!((sn / n).abs() < 0.03);
        assert!((sn2 / n - 1.0).abs() < 0.05);
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(3);
        let mut seen = [false; 5];
        for _ in 0..500 {
            seen[r.below(5)] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
