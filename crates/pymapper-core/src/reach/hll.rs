//! HyperLogLog distinct counting (own implementation, design: dependency policy).
//!
//! Elements are 32-bit keys ([`key`]): a 64-bit bijective mix of the element's identity,
//! truncated to its top 32 bits. A dense sketch has `m = 2^p` one-byte registers; key `k` goes
//! to register `k >> (32 - p)` with rank `1 + leading zeros of the remaining q = 32 - p bits`
//! (capped at `q + 1`). Merging is a register-wise max, so a sketch of a union is the max of
//! the sketches, whatever the order and with duplicates counted once.
//!
//! # Estimator
//!
//! [`estimate`] is Ertl's improved estimator ("New cardinality estimation algorithms for
//! HyperLogLog sketches", 2017, algorithm 6): unbiased over the whole range (small
//! cardinalities included, where classic HLL needs linear counting and bias tables) and it
//! accounts for saturated registers. Relative standard error `1.04 / sqrt(m)`
//! ([`rel_std_error`]): 6.5% at `p = 8`, 3.25% at `p = 10`, 1.63% at `p = 12`.
//!
//! # 32-bit keys
//!
//! Keys of distinct elements collide with probability `2^-32` per pair, so a set of `n`
//! elements has about `n^2 / 2^33` fewer distinct keys: a relative bias of `-n / 2^33`
//! (-0.01% at 1M elements, -0.2% at 16M), small against the sketch error. In exchange sparse
//! sets (see `sketch.rs`) store 4 bytes per element and dense sketches never need a second
//! hash.

/// Smallest supported precision (16 registers).
pub const MIN_PRECISION: u8 = 4;
/// Largest supported precision (65,536 registers).
pub const MAX_PRECISION: u8 = 16;

/// The splitmix64 finaliser: a bijection on `u64` with full avalanche.
#[inline]
pub fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Key of the element `(a, b)`: distinct pairs give independent, uniformly distributed keys
/// (up to the 32-bit truncation). `b` distinguishes several elements of one owner `a` (for
/// instance the lines of one node).
#[inline]
pub fn key(a: u32, b: u32) -> u32 {
    (mix64(((u64::from(a) << 32) | u64::from(b)) ^ 0x9e37_79b9_7f4a_7c15) >> 32) as u32
}

/// Number of registers at precision `p`.
#[inline]
pub fn registers(p: u8) -> usize {
    1usize << p
}

/// Relative standard error of a dense sketch at precision `p`: `1.04 / sqrt(2^p)`.
pub fn rel_std_error(p: u8) -> f64 {
    1.04 / (registers(p) as f64).sqrt()
}

/// Adds `key` to the registers of a precision-`p` sketch (`regs.len() == 2^p`).
#[inline]
pub fn insert(regs: &mut [u8], p: u8, key: u32) {
    let idx = (key >> (32 - u32::from(p))) as usize;
    let rest = key << p;
    let q = 32 - u32::from(p);
    let rank = (rest.leading_zeros() + 1).min(q + 1) as u8;
    let r = &mut regs[idx];
    if rank > *r {
        *r = rank;
    }
}

/// `dst = max(dst, src)` register-wise (the sketch of the union). Equal lengths.
#[inline]
pub fn merge(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = (*d).max(s);
    }
}

/// Estimated number of distinct keys added to the registers of a precision-`p` sketch.
pub fn estimate(regs: &[u8], p: u8) -> f64 {
    let q = 32 - usize::from(p);
    let m = regs.len() as f64;
    let mut hist = [0u32; 34];
    for &r in regs {
        hist[usize::from(r).min(q + 1)] += 1;
    }
    if hist[q + 1] as usize == regs.len() {
        // Every register saturated (far beyond the 32-bit key range): the estimator diverges;
        // report the top of the representable range instead.
        return m * (1u64 << (q + 1)) as f64;
    }
    let mut z = m * tau(1.0 - f64::from(hist[q + 1]) / m);
    for k in (1..=q).rev() {
        z = 0.5 * (z + f64::from(hist[k]));
    }
    z += m * sigma(f64::from(hist[0]) / m);
    let alpha = 1.0 / (2.0 * std::f64::consts::LN_2);
    alpha * m * m / z
}

/// Ertl's `sigma(x) = x + sum_{k>=1} x^(2^k) 2^(k-1)`.
fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let prev = z;
        z += x * y;
        y += y;
        if z == prev {
            return z;
        }
    }
}

/// Ertl's `tau(x) = (1 - x - sum_{k>=1} (1 - x^(2^-k))^2 2^-k) / 3`.
fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let prev = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if z == prev {
            return z / 3.0;
        }
    }
}

/// A standalone dense HyperLogLog sketch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hll {
    p: u8,
    regs: Box<[u8]>,
}

impl Hll {
    /// An empty sketch with `2^p` registers.
    ///
    /// # Panics
    /// If `p` is outside [`MIN_PRECISION`]`..=`[`MAX_PRECISION`].
    pub fn new(p: u8) -> Hll {
        assert!((MIN_PRECISION..=MAX_PRECISION).contains(&p), "HLL precision {p} out of range");
        Hll { p, regs: vec![0u8; registers(p)].into_boxed_slice() }
    }

    #[inline]
    pub fn precision(&self) -> u8 {
        self.p
    }

    /// The registers.
    #[inline]
    pub fn registers(&self) -> &[u8] {
        &self.regs
    }

    #[inline]
    pub fn insert(&mut self, key: u32) {
        insert(&mut self.regs, self.p, key);
    }

    /// Adds every key of `other` (same precision).
    ///
    /// # Panics
    /// If the precisions differ.
    pub fn merge(&mut self, other: &Hll) {
        assert_eq!(self.p, other.p, "merging HLL sketches of different precision");
        merge(&mut self.regs, &other.regs);
    }

    /// Estimated number of distinct keys.
    pub fn estimate(&self) -> f64 {
        estimate(&self.regs, self.p)
    }

    pub fn clear(&mut self) {
        self.regs.fill(0);
    }
}
