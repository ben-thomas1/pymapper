//! Line index of one source: the offset of every line start, built in one vectorised pass,
//! and a cursor that maps byte offsets to `(line, column)`.
//!
//! Lines end at `\n`, `\r\n` (one line end, after the `\n`) and a lone `\r`. The index
//! holds the offset just after every line end, ascending. Building it costs one pass over
//! the source (SIMD on aarch64 / x86_64, word-at-a-time elsewhere); a query is then a short
//! walk from the previous query's line (queries are mostly increasing and close together),
//! falling back to a binary search for long jumps.
//!
//! The index buffer is reused across files through a per-thread cache, so steady-state
//! extraction allocates nothing for it.

use std::cell::Cell;

use super::events::Pos;

/// Appends to `out` the offset after every line end in `src` (see the module docs).
/// Offsets fit `u32` because the lexer truncates sources to [`crate::lex::MAX_SOURCE_LEN`].
pub(crate) fn line_starts(src: &[u8], out: &mut Vec<u32>) {
    debug_assert!(src.len() <= crate::lex::MAX_SOURCE_LEN);
    let from = out.len();
    if lf_starts(src, out) {
        // A `\r` somewhere: redo this source with the exact (byte-wise) rules.
        out.truncate(from);
        line_starts_ref(src, out);
    }
}

/// Reference implementation: one byte at a time. Used for sources containing `\r`, and as
/// the oracle for the vectorised scanners in tests.
pub(crate) fn line_starts_ref(src: &[u8], out: &mut Vec<u32>) {
    for (i, &b) in src.iter().enumerate() {
        if b == b'\n' || (b == b'\r' && src.get(i + 1) != Some(&b'\n')) {
            out.push(i as u32 + 1);
        }
    }
}

/// Number of line ends in `s` (same rules as [`line_starts`]). Counting `\n` is a plain
/// loop the compiler vectorises; a `\r` (rare) takes the byte-wise path.
#[inline]
pub(crate) fn count_line_ends(s: &[u8]) -> usize {
    if s.contains(&b'\r') {
        return (0..s.len()).filter(|&i| s[i] == b'\n' || (s[i] == b'\r' && s.get(i + 1) != Some(&b'\n'))).count();
    }
    s.iter().filter(|&&b| b == b'\n').count()
}

/// Appends the offset after every `\n` of `src`; returns whether `src` contains a `\r` (then
/// the output is incomplete and the caller must use [`line_starts_ref`]).
#[inline]
fn lf_starts(src: &[u8], out: &mut Vec<u32>) -> bool {
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        // SAFETY: compiled only when NEON is enabled for the target (it is part of the
        // aarch64 baseline), so the CPU running this code supports it.
        unsafe { lf_starts_neon(src, out) }
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        // SAFETY: compiled only when SSE2 is enabled for the target (it is part of the
        // x86_64 baseline), so the CPU running this code supports it.
        unsafe { lf_starts_sse2(src, out) }
    }
    #[cfg(not(any(all(target_arch = "aarch64", target_feature = "neon"), all(target_arch = "x86_64", target_feature = "sse2"))))]
    {
        lf_starts_swar(src, out, 0)
    }
}

/// Pushes `base + k + 1` for every flagged byte `k` of mask `m`, lowest first. Each byte has
/// at most one bit set in `m`, and that bit's index `>> shift` is `k`.
#[inline(always)]
fn push_bits(out: &mut Vec<u32>, base: usize, mut m: u64, shift: u32) {
    while m != 0 {
        let k = (m.trailing_zeros() >> shift) as usize;
        out.push((base + k + 1) as u32);
        m &= m - 1;
    }
}

/// Portable word-at-a-time scanner from `from` (also the tail of the SIMD scanners).
fn lf_starts_swar(src: &[u8], out: &mut Vec<u32>, from: usize) -> bool {
    const LO7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    const NL: u64 = 0x0a0a_0a0a_0a0a_0a0a;
    const CR: u64 = 0x0d0d_0d0d_0d0d_0d0d;
    // High bit set exactly in the zero bytes of `x` (no false positives).
    let zeros = |x: u64| !(((x & LO7).wrapping_add(LO7)) | x | LO7);
    let mut cr = 0u64;
    let mut i = from;
    while i + 8 <= src.len() {
        let mut a = [0u8; 8];
        a.copy_from_slice(&src[i..i + 8]);
        let w = u64::from_le_bytes(a);
        cr |= zeros(w ^ CR);
        // One bit per byte (bit 7 of the byte): bit index / 8 is the byte.
        push_bits(out, i, zeros(w ^ NL), 3);
        i += 8;
    }
    let mut saw_cr = cr != 0;
    for (k, &b) in src[i..].iter().enumerate() {
        if b == b'\n' {
            out.push((i + k + 1) as u32);
        }
        saw_cr |= b == b'\r';
    }
    saw_cr
}

/// NEON scanner: 16 bytes per step; the byte mask is narrowed to 4 bits per byte.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[target_feature(enable = "neon")]
fn lf_starts_neon(src: &[u8], out: &mut Vec<u32>) -> bool {
    use std::arch::aarch64::{
        vceqq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vmaxvq_u8, vorrq_u8, vreinterpret_u64_u8,
        vreinterpretq_u16_u8, vshrn_n_u16,
    };
    let nl = vdupq_n_u8(b'\n');
    let cr = vdupq_n_u8(b'\r');
    let mut cr_any = vdupq_n_u8(0);
    let mut i = 0;
    while i + 16 <= src.len() {
        // SAFETY: `i + 16 <= src.len()`, so the 16 bytes read are inside `src`; `vld1q_u8`
        // has no alignment requirement.
        let v = unsafe { vld1q_u8(src.as_ptr().add(i)) };
        let eq = vceqq_u8(v, nl);
        cr_any = vorrq_u8(cr_any, vceqq_u8(v, cr));
        // Each byte lane (0x00 / 0xff) becomes a nibble of a 64-bit mask; keep one bit per
        // nibble so `push_bits` counts bytes with `>> 2`.
        let m = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(eq))));
        push_bits(out, i, m & 0x8888_8888_8888_8888, 2);
        i += 16;
    }
    let saw_cr = vmaxvq_u8(cr_any) != 0;
    lf_starts_swar(src, out, i) | saw_cr
}

/// SSE2 scanner (baseline on x86_64, no runtime detection needed): 16 bytes per step.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[target_feature(enable = "sse2")]
fn lf_starts_sse2(src: &[u8], out: &mut Vec<u32>) -> bool {
    use std::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128, _mm_set1_epi8, _mm_setzero_si128};
    let nl = _mm_set1_epi8(b'\n' as i8);
    let cr = _mm_set1_epi8(b'\r' as i8);
    let mut cr_any = _mm_setzero_si128();
    let mut i = 0;
    while i + 16 <= src.len() {
        // SAFETY: `i + 16 <= src.len()`, so the 16 bytes read are inside `src`;
        // `_mm_loadu_si128` has no alignment requirement.
        let v = unsafe { _mm_loadu_si128(src.as_ptr().add(i).cast::<__m128i>()) };
        cr_any = _mm_or_si128(cr_any, _mm_cmpeq_epi8(v, cr));
        let m = _mm_movemask_epi8(_mm_cmpeq_epi8(v, nl)) as u32;
        push_bits(out, i, u64::from(m), 0);
        i += 16;
    }
    let saw_cr = _mm_movemask_epi8(cr_any) != 0;
    lf_starts_swar(src, out, i) | saw_cr
}

/// Largest index buffer (entries) kept in the per-thread cache; a huge file's buffer is freed.
const MAX_CACHED: usize = 1 << 20;

thread_local! {
    /// Line-start buffer of the last finished [`LineCursor`] on this thread.
    static STARTS: Cell<Vec<u32>> = const { Cell::new(Vec::new()) };
}

/// Maps byte offsets of one source to [`Pos`] through its line index.
pub(crate) struct LineCursor {
    /// Offset after every line end, ascending (line `k + 2` starts at `starts[k]`).
    starts: Vec<u32>,
    /// Number of line starts at or before the last queried offset.
    idx: usize,
    len: u32,
}

impl LineCursor {
    pub(crate) fn new(src: &[u8]) -> Self {
        let mut starts = STARTS.try_with(Cell::take).unwrap_or_default();
        starts.clear();
        line_starts(src, &mut starts);
        LineCursor { starts, idx: 0, len: src.len() as u32 }
    }

    /// Line (1-based) and column (bytes from the line start) of offset `off`. Offsets past
    /// the end are clamped to it.
    #[inline]
    pub(crate) fn pos(&mut self, off: u32) -> Pos {
        let end = off.min(self.len);
        let s = &self.starts[..];
        let mut i = self.idx;
        if i < s.len() && s[i] <= end {
            // Forward: a few steps, then a binary search over the rest.
            i += 1;
            let mut steps = 0;
            while i < s.len() && s[i] <= end {
                i += 1;
                steps += 1;
                if steps == 8 {
                    i += s[i..].partition_point(|&x| x <= end);
                    break;
                }
            }
        } else if i > 0 && s[i - 1] > end {
            // Backward (decorator starts, block ends after lookahead): binary search.
            i = s[..i].partition_point(|&x| x <= end);
        }
        self.idx = i;
        let line_start = if i == 0 { 0 } else { s[i - 1] };
        Pos { line: i as u32 + 1, col: end - line_start }
    }
}

impl Drop for LineCursor {
    fn drop(&mut self) {
        if self.starts.capacity() <= MAX_CACHED {
            let v = std::mem::take(&mut self.starts);
            // During thread teardown the cache may be gone: then the buffer is just freed.
            let _ = STARTS.try_with(|c| c.set(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast(src: &[u8]) -> Vec<u32> {
        let mut v = vec![7];
        line_starts(src, &mut v);
        assert_eq!(v[0], 7, "existing entries are kept");
        v.remove(0);
        v
    }

    fn reference(src: &[u8]) -> Vec<u32> {
        let mut v = Vec::new();
        line_starts_ref(src, &mut v);
        v
    }

    #[test]
    fn simd_and_swar_match_reference_at_every_offset_and_length() {
        // Line ends at every lane position, around the 8/16-byte block edges and the tail.
        for len in 0..80 {
            for pos in 0..len.max(1) {
                for pat in [&b"\n"[..], b"\n\n", b"\r\n", b"\r", b"\n\r"] {
                    let mut src = vec![b'x'; len];
                    for (k, &b) in pat.iter().enumerate() {
                        if pos + k < len {
                            src[pos + k] = b;
                        }
                    }
                    assert_eq!(fast(&src), reference(&src), "{src:?}");
                    let mut swar = Vec::new();
                    let cr = lf_starts_swar(&src, &mut swar, 0);
                    assert_eq!(cr, src.contains(&b'\r'));
                    if !cr {
                        assert_eq!(swar, reference(&src));
                    }
                }
            }
        }
    }

    #[test]
    fn all_byte_values_and_dense_newlines() {
        // Pseudo-random bytes: values next to `\n`/`\r` (0x09, 0x0b, 0x0c, 0x0e, 0x8a, 0xff)
        // must not match.
        let src: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        assert_eq!(fast(&src), reference(&src));
        let no_cr: Vec<u8> = src.iter().map(|&b| if b == b'\r' { b'a' } else { b }).collect();
        assert_eq!(fast(&no_cr), reference(&no_cr));
        let mut swar = Vec::new();
        assert!(!lf_starts_swar(&no_cr, &mut swar, 0));
        assert_eq!(swar, reference(&no_cr));
        let all: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
        assert_eq!(fast(&all), reference(&all));
        let dense = vec![b'\n'; 100];
        assert_eq!(fast(&dense), (1..=100).collect::<Vec<u32>>());
        assert_eq!(fast(b""), Vec::<u32>::new());
        assert_eq!(fast(b"a\r\nb\rc\n"), vec![3, 5, 7]);
    }

    #[test]
    fn count_line_ends_matches_reference() {
        for src in [&b""[..], b"\n", b"\r", b"\r\n", b"a\r\nb\rc\n\n", b"\r\r\n\n\r"] {
            assert_eq!(count_line_ends(src), reference(src).len(), "{src:?}");
        }
        let src: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        assert_eq!(count_line_ends(&src), reference(&src).len());
        let no_cr: Vec<u8> = src.iter().map(|&b| if b == b'\r' { b'a' } else { b }).collect();
        assert_eq!(count_line_ends(&no_cr), reference(&no_cr).len());
    }

    #[test]
    fn cursor_reuses_the_thread_buffer() {
        let a = LineCursor::new(b"a\nb\nc\n");
        let cap = a.starts.capacity();
        drop(a);
        let mut b = LineCursor::new(b"x");
        assert!(b.starts.capacity() >= cap, "buffer came back from the cache");
        assert_eq!(b.pos(1), Pos { line: 1, col: 1 });
        assert_eq!(b.pos(99), Pos { line: 1, col: 1 });
    }

    #[test]
    fn cursor_long_jumps_both_ways() {
        let src: Vec<u8> = b"ab\n".repeat(1000);
        let mut c = LineCursor::new(&src);
        for &off in &[0u32, 2999, 3, 1500, 1501, 1499, 3000, 0, 2, 40, 41, 70, 3000, 2998] {
            let line = (off / 3 + 1).min(1001);
            assert_eq!(c.pos(off), Pos { line, col: off - (line - 1) * 3 }, "@{off}");
        }
    }
}
