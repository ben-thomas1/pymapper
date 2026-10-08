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
