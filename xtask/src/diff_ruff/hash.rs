//! Fx-style hasher (the rustc / Firefox multiply-rotate hash) for the diff engine's indexes.
//!
//! Not DoS resistant, which is fine for dev tooling over trusted inputs. Written here because
//! the dependency policy says "write our own" for hashers.

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Streaming Fx hasher. Feeds 8 bytes per step.
#[derive(Clone, Copy, Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rem) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        if !rem.is_empty() {
            let mut w = [0u8; 8];
            w[..rem.len()].copy_from_slice(rem);
            self.add(u64::from_le_bytes(w));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// `BuildHasher` for `HashMap<_, _, FxBuild>`.
pub type FxBuild = BuildHasherDefault<FxHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_bytes(b: &[u8]) -> u64 {
        let mut h = FxHasher::default();
        h.write(b);
        h.finish()
    }

    #[test]
    fn deterministic_and_discriminating() {
        assert_eq!(hash_bytes(b"self.assertEqual"), hash_bytes(b"self.assertEqual"));
        assert_ne!(hash_bytes(b"a.b"), hash_bytes(b"a.c"));
        // Remainder bytes participate.
        assert_ne!(hash_bytes(b"abcdefgh1"), hash_bytes(b"abcdefgh2"));
    }
}
