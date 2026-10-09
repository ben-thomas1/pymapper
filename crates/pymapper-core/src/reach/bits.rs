//! A fixed-size bit set over `0..len` (one bit per node).

/// Bits `0..len`, 64 per word.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
    len: u32,
}

impl BitSet {
    /// `len` cleared bits.
    pub fn new(len: u32) -> BitSet {
        BitSet { words: vec![0; (len as usize).div_ceil(64)], len }
    }

    /// Number of bits (set or not).
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether bit `i` is set.
    #[inline]
    pub fn contains(&self, i: u32) -> bool {
        self.words[(i >> 6) as usize] & (1 << (i & 63)) != 0
    }

    /// Sets bit `i`.
    #[inline]
    pub fn insert(&mut self, i: u32) {
        assert!(i < self.len, "bit {i} out of range for {}", self.len);
        self.words[(i >> 6) as usize] |= 1 << (i & 63);
    }

    /// Sets bit `i`; returns whether it was clear.
    #[inline]
    pub fn insert_new(&mut self, i: u32) -> bool {
        assert!(i < self.len, "bit {i} out of range for {}", self.len);
        let w = &mut self.words[(i >> 6) as usize];
        let b = 1 << (i & 63);
        let new = *w & b == 0;
        *w |= b;
        new
    }

    /// Clears bit `i`.
    #[inline]
    pub fn remove(&mut self, i: u32) {
        self.words[(i >> 6) as usize] &= !(1 << (i & 63));
    }

    /// Number of set bits.
    pub fn count(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }

    /// Clears every bit.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// Set bits, ascending.
    pub fn ones(&self) -> Ones<'_> {
        Ones { words: &self.words, base: 0, cur: self.words.first().copied().unwrap_or(0) }
    }

    /// The words (bit `i` is bit `i % 64` of word `i / 64`; bits past `len` are clear).
    #[inline]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity() * 8
    }
}

/// Iterator over the set bits of a [`BitSet`], ascending.
#[derive(Clone)]
pub struct Ones<'a> {
    words: &'a [u64],
    /// Index of the word in `cur`.
    base: usize,
    /// Remaining bits of word `base`.
    cur: u64,
}

impl Iterator for Ones<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        while self.cur == 0 {
            self.base += 1;
            if self.base >= self.words.len() {
                return None;
            }
            self.cur = self.words[self.base];
        }
        let bit = self.cur.trailing_zeros();
        self.cur &= self.cur - 1;
        Some((self.base as u32) * 64 + bit)
    }
}
