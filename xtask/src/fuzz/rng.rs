//! Deterministic, seedable RNG: xorshift64* seeded through splitmix64.
//!
//! Every fuzz input is generated from its own RNG stream, `Rng::for_iteration(seed, iter)`, so
//! any input can be regenerated from `(seed, iter)` alone (the watchdog relies on this to save
//! hanging inputs without copying every input).

/// One splitmix64 step: a bijective, well-mixing hash of `x`.
#[inline]
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// xorshift64* generator. Never holds a zero state.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator seeded from any `u64` (zero included).
    pub fn new(seed: u64) -> Rng {
        let s = splitmix64(seed);
        Rng { state: if s == 0 { 0x2545_F491_4F6C_DD1D } else { s } }
    }

    /// The independent stream for fuzz iteration `iter` of a run seeded with `seed`.
    pub fn for_iteration(seed: u64, iter: u64) -> Rng {
        Rng::new(seed ^ splitmix64(iter ^ 0xD6E8_FEB8_6659_FD93))
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (Lemire's multiply-shift; the tiny bias is irrelevant here).
    /// Returns 0 when `n == 0`.
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// True with probability `1 / n` (always true for `n <= 1`).
    #[inline]
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    /// A length skewed towards small values: uniform in `1..=2^k` for a uniform `k` in `0..=bits`.
    #[inline]
    pub fn small_len(&mut self, bits: u32) -> usize {
        let k = self.below(bits as usize + 1);
        1 + self.below(1usize << k)
    }

    #[inline]
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn zero_seed_is_fine_and_streams_differ() {
        let mut z = Rng::new(0);
        let first = z.next_u64();
        assert_ne!(first, 0);
        assert_ne!(Rng::new(1).next_u64(), first);
        let mut seen = std::collections::HashSet::new();
        for iter in 0..10_000u64 {
            assert!(seen.insert(Rng::for_iteration(7, iter).next_u64()), "iteration streams collide");
        }
        assert_ne!(Rng::for_iteration(7, 3).next_u64(), Rng::for_iteration(8, 3).next_u64());
    }

    #[test]
    fn known_values_are_stable() {
        // Pin the stream so a refactor cannot silently change which inputs a seed means.
        assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
        let mut r = Rng::new(0);
        assert_eq!([r.next_u64(), r.next_u64()], [8916199331640804048, 16032783972208265725]);
        assert_eq!(Rng::for_iteration(1, 2).next_u64(), 43534307035317356);
    }

    #[test]
    fn below_and_range_bounds() {
        let mut r = Rng::new(9);
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
        let mut hist = [0u32; 7];
        for _ in 0..70_000 {
            let v = r.below(7);
            hist[v] += 1;
            let w = 3 + r.below(3);
            assert!((3..=5).contains(&w));
            let l = r.small_len(6);
            assert!((1..=64).contains(&l));
        }
        for h in hist {
            assert!((9_000..11_000).contains(&h), "skewed histogram {hist:?}");
        }
        assert!(r.one_in(1));
    }
}
