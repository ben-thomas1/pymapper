//! Byte tables and word-at-a-time (SWAR) search helpers. Portable scalar code, no `unsafe`.

/// Identifier-continue bytes: ASCII letters, digits, `_`, and every byte >= 0x80
/// (non-ASCII identifiers are accepted byte-wise, without UTF-8 validation or NFKC).
pub(super) static IDENT: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        t[i] = b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
        i += 1;
    }
    t
};

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// High bit set in each byte lane of `w` that equals `b`. Lanes above the first
/// match may hold false positives (borrow propagation), so callers only use the
/// lowest set bit.
#[inline(always)]
fn eq_mask(w: u64, b: u8) -> u64 {
    let x = w ^ (LO * b as u64);
    x.wrapping_sub(LO) & !x & HI
}

#[inline(always)]
fn load(h: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&h[i..i + 8]);
    u64::from_le_bytes(a)
}

/// High bit set in each lane of `x` (all lanes < 0x80) whose byte is in `lo..=hi`.
/// No carries cross lanes: each lane sum stays <= 0xff.
#[inline(always)]
fn in_range(x: u64, lo: u8, hi: u8) -> u64 {
    let ge = x.wrapping_add(LO * (0x80 - lo) as u64);
    let gt = x.wrapping_add(LO * (0x7f - hi) as u64);
    ge & !gt & HI
}

/// First index `>= i` whose byte is not an identifier byte (see [`IDENT`]), or `h.len()`.
#[inline(always)]
pub(super) fn skip_ident(h: &[u8], mut i: usize) -> usize {
    while i + 8 <= h.len() {
        let w = load(h, i);
        let x = w & !HI;
        let ident = in_range(x | (LO * 0x20), b'a', b'z') | in_range(x, b'0', b'9') | in_range(x, b'_', b'_') | (w & HI);
        let stop = !ident & HI;
        if stop != 0 {
            return i + (stop.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < h.len() && IDENT[h[i] as usize] {
        i += 1;
    }
    i
}

/// First index `>= i` whose byte is not `b' '`, or `h.len()`.
#[inline(always)]
pub(super) fn skip_spaces(h: &[u8], mut i: usize) -> usize {
    while i + 8 <= h.len() {
        // Lanes that differ from a space are nonzero after the XOR; find the first one exactly
        // (no borrow trick, so no false positives in any lane).
        let x = load(h, i) ^ (LO * b' ' as u64);
        if x != 0 {
            return i + (x.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < h.len() && h[i] == b' ' {
        i += 1;
    }
    i
}

/// First index `>= from` whose byte is `a` or `b`.
#[inline]
pub(super) fn find2(h: &[u8], mut i: usize, a: u8, b: u8) -> Option<usize> {
    while i + 8 <= h.len() {
        let w = load(h, i);
        let m = eq_mask(w, a) | eq_mask(w, b);
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < h.len() {
        let c = h[i];
        if c == a || c == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// First index `>= from` whose byte is one of `a`, `b`, `c`, `d`.
#[inline]
pub(super) fn find4(h: &[u8], mut i: usize, a: u8, b: u8, c: u8, d: u8) -> Option<usize> {
    while i + 8 <= h.len() {
        let w = load(h, i);
        let m = eq_mask(w, a) | eq_mask(w, b) | eq_mask(w, c) | eq_mask(w, d);
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < h.len() {
        let x = h[i];
        if x == a || x == b || x == c || x == d {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(h: &[u8], from: usize, set: &[u8]) -> Option<usize> {
        (from..h.len()).find(|&i| set.contains(&h[i]))
    }

    #[test]
    fn find_matches_naive_on_all_offsets() {
        // Includes 0x80/0xff/0x00 lanes that stress the borrow trick.
        let hay: Vec<u8> = (0..300u32).map(|i| [b'a', 0x80, 0xff, 0x00, b'"', b'\n', 0x7f, b'\\'][(i * 7 % 8) as usize]).collect();
        for len in 0..hay.len() {
            let h = &hay[..len];
            for from in 0..=len {
                assert_eq!(find2(h, from, b'"', b'\\'), naive(h, from, b"\"\\"), "len {len} from {from}");
                assert_eq!(find4(h, from, b'"', b'\\', b'\n', b'\r'), naive(h, from, b"\"\\\n\r"));
                assert_eq!(find2(h, from, 0x00, 0x81), naive(h, from, &[0x00, 0x81]));
            }
        }
    }

    #[test]
    fn skip_ident_and_spaces_match_naive() {
        // Every byte value at every lane position, before and after an identifier run.
        for b in 0..=255u8 {
            for pos in 0..20 {
                let mut h = vec![b'a'; 20];
                h[pos] = b;
                let want = if IDENT[b as usize] { 20 } else { pos };
                assert_eq!(skip_ident(&h, 0), want, "byte {b:#x} at {pos}");
                let mut s = vec![b' '; 20];
                s[pos] = b;
                assert_eq!(skip_spaces(&s, 0), if b == b' ' { 20 } else { pos }, "byte {b:#x} at {pos}");
            }
        }
        assert_eq!(skip_ident(b"", 0), 0);
        assert_eq!(skip_spaces(b"  ", 2), 2);
        assert_eq!(skip_ident(b"abc def", 1), 3);
    }

    #[test]
    fn find_each_lane_position() {
        for pos in 0..24 {
            let mut h = vec![b'x'; 24];
            h[pos] = b'\n';
            assert_eq!(find2(&h, 0, b'\n', b'\r'), Some(pos));
            // A match one lane below (0x01 less) must not trigger a false positive.
            h[pos] = b'\n' - 1;
            assert_eq!(find2(&h, 0, b'\n', b'\r'), None);
        }
    }
}
