//! Rolling k-gram hashes and winnowing (Schleimer, Wilkerson, Aiken: "Winnowing: local
//! algorithms for document fingerprinting", SIGMOD 2003), fed by the parser's token events.

use super::DupesParams;
use super::norm::Normaliser;
use crate::extract::Visitor;
use crate::lex::{Lexer, Token, TokenKind};

/// Base of the polynomial rolling hash (odd, so multiplication mod 2^64 is invertible).
pub(super) const BASE: u64 = 0x100_0000_01b3;

/// One selected fingerprint (12 bytes): the low 32 bits of the mixed k-gram hash and where the
/// k-gram starts (index into the file's normalised token sequence).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint {
    pub hash: u32,
    pub file: u32,
    pub pos: u32,
}

const _: () = assert!(std::mem::size_of::<Fingerprint>() == 12);

/// Fingerprints of many files plus the parameters they were taken with. Build one per thread
/// (rows of one file are contiguous) and concatenate with [`append`](Self::append).
#[derive(Clone, Debug, Default)]
pub struct Fingerprints {
    pub(crate) rows: Vec<Fingerprint>,
}

impl Fingerprints {
    pub fn new() -> Fingerprints {
        Fingerprints::default()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[inline]
    pub fn as_slice(&self) -> &[Fingerprint] {
        &self.rows
    }

    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Fingerprint> {
        self.rows.iter()
    }

    /// Moves every row of `other` to the end of `self`.
    pub fn append(&mut self, other: &mut Fingerprints) {
        self.rows.append(&mut other.rows);
    }

    pub fn reserve_exact(&mut self, n: usize) {
        self.rows.reserve_exact(n);
    }

    pub fn shrink_to_fit(&mut self) {
        self.rows.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn heap_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Fingerprint>()
    }
}

/// Per-thread reusable buffers of fingerprinting.
#[derive(Debug, Default)]
pub struct FingerprintScratch {
    /// Normalised codes of the current file.
    codes: Vec<u8>,
    /// Mixed k-gram hashes of the current file.
    hashes: Vec<u64>,
    /// Sliding-window-minimum deque (indices into `hashes`).
    deque: Vec<u32>,
}

impl FingerprintScratch {
    pub fn new() -> FingerprintScratch {
        FingerprintScratch::default()
    }
}

/// Parser visitor collecting the normalised tokens of one file; [`finish`](Self::finish)
/// winnows them. Tee it next to the facts/metrics visitors so fingerprinting costs no extra
/// pass over the source.
pub struct FingerprintVisitor<'a> {
    src: &'a [u8],
    norm: Normaliser,
    scratch: &'a mut FingerprintScratch,
}

impl<'a> FingerprintVisitor<'a> {
    pub fn new(src: &'a [u8], scratch: &'a mut FingerprintScratch) -> FingerprintVisitor<'a> {
        scratch.codes.clear();
        FingerprintVisitor { src, norm: Normaliser::default(), scratch }
    }

    /// Winnows the collected tokens and appends the fingerprints of `file` to `out`.
    pub fn finish(self, file: u32, params: &DupesParams, out: &mut Fingerprints) {
        let s = self.scratch;
        winnow(&s.codes, params, &mut s.hashes, &mut s.deque, file, out);
    }
}

impl Visitor for FingerprintVisitor<'_> {
    #[inline]
    fn token(&mut self, tok: Token) {
        if let Some(c) = self.norm.push(self.src, tok) {
            self.scratch.codes.push(c);
        }
    }
}

/// Feeds the lexer's non-structural tokens of `src` to `f`: the same stream the parser
/// reports through [`Visitor::token`] for well-formed input.
#[inline]
pub(crate) fn for_each_token(src: &[u8], mut f: impl FnMut(Token)) {
    let mut lx = Lexer::new(src);
    loop {
        let t = lx.next_token();
        match t.kind {
            TokenKind::EndOfFile => break,
            TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent => {}
            k if k.is_error() || k.is_trivia() => {}
            _ => f(t),
        }
    }
}

/// Fingerprints `src` with the lexer instead of the parser (same result for well-formed
/// input); for callers that do not run the extractor.
pub fn fingerprint_source(src: &[u8], file: u32, params: &DupesParams, scratch: &mut FingerprintScratch, out: &mut Fingerprints) {
    scratch.codes.clear();
    let mut norm = Normaliser::default();
    let codes = &mut scratch.codes;
    for_each_token(src, |t| {
        if let Some(c) = norm.push(src, t) {
            codes.push(c);
        }
    });
    winnow(&scratch.codes, params, &mut scratch.hashes, &mut scratch.deque, file, out);
}

/// Final mixer of the rolling hash (murmur3 fmix64): spreads the polynomial hash so the window
/// minimum is a uniform sample.
#[inline(always)]
pub(super) fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// Hashes every k-gram of `codes` and selects the rightmost minimum of every window of `w`
/// consecutive hashes (robust winnowing: a fingerprint is recorded once even when it stays
/// the minimum of many windows). Any token run of at least `k + w - 1` shared by two files
/// yields at least one common fingerprint. Files shorter than that yield none.
pub(crate) fn winnow(
    codes: &[u8],
    params: &DupesParams,
    hashes: &mut Vec<u64>,
    deque: &mut Vec<u32>,
    file: u32,
    out: &mut Fingerprints,
) {
    let (k, w) = (params.k() as usize, params.w() as usize);
    if codes.len() < k + w - 1 {
        return;
    }
    // Rolling polynomial hash: h = sum c_i * BASE^(k-1-i) over the window (mod 2^64).
    let top = (1..k).fold(1u64, |p, _| p.wrapping_mul(BASE));
    hashes.clear();
    hashes.reserve(codes.len() - k + 1);
    let mut h = 0u64;
    for &c in &codes[..k] {
        h = h.wrapping_mul(BASE).wrapping_add(u64::from(c) + 1);
    }
    hashes.push(mix(h));
    for i in k..codes.len() {
        let out_c = u64::from(codes[i - k]) + 1;
        h = h.wrapping_sub(out_c.wrapping_mul(top)).wrapping_mul(BASE).wrapping_add(u64::from(codes[i]) + 1);
        hashes.push(mix(h));
    }
    // Sliding window minimum with a monotonic deque: hashes[deque[head..]] increase strictly,
    // so the front is the rightmost minimum of the window.
    deque.clear();
    let mut head = 0usize;
    let mut last = u32::MAX;
    for (i, &hv) in hashes.iter().enumerate() {
        while deque.len() > head && hashes[deque[deque.len() - 1] as usize] >= hv {
            deque.pop();
        }
        deque.push(i as u32);
        if (deque[head] as usize) + w <= i {
            head += 1;
        }
        if i + 1 >= w {
            let m = deque[head];
            if m != last {
                last = m;
                out.rows.push(Fingerprint { hash: hashes[m as usize] as u32, file, pos: m });
            }
        }
        // Compact the deque now and then so it stays O(w).
        if head > 1024 && head * 2 > deque.len() {
            deque.drain(..head);
            head = 0;
        }
    }
}
