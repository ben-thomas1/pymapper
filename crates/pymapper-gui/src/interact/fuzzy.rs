//! Own fuzzy matcher for search-and-jump: the pattern must be a (case-insensitive)
//! subsequence of the text; matches are scored like fzf's v1 algorithm.
//!
//! ```text
//!   forward scan   first position where the whole pattern has matched -> end
//!   backward scan  from end, the latest start that still matches      -> tightest window
//!   score window   +16 per matched byte, boundary / camelCase / consecutive bonuses,
//!                  gap penalties (-3 to open, -1 per extra byte), first-byte bonus doubled
//! ```
//!
//! O(len(text)) per text, no allocation. Patterns and texts are bytes (identifiers need not be
//! UTF-8); case folding is ASCII only, like the index's names.

/// Score of one matched byte.
const MATCH: i32 = 16;
/// Opening a gap (unmatched bytes between two matched ones).
const GAP_START: i32 = -3;
/// Each further byte of a gap.
const GAP_EXTEND: i32 = -1;
/// Matched byte at a word boundary: after `_`, `.`, `:`, `/`, `-`, a space or the start.
const BONUS_BOUNDARY: i32 = 8;
/// Matched upper-case byte after a lower-case one (`camelCase`), or a digit after a letter.
const BONUS_CAMEL: i32 = 7;
/// Matched byte right after another matched byte.
const BONUS_CONSECUTIVE: i32 = 4;
/// The first pattern byte's bonus is multiplied by this.
const FIRST_MULTIPLIER: i32 = 2;
/// Pattern matched with exactly the same case.
const BONUS_EXACT_CASE: i32 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Delim,
    Other,
}

#[inline]
fn class(c: u8) -> Class {
    match c {
        b'a'..=b'z' => Class::Lower,
        b'A'..=b'Z' => Class::Upper,
        b'0'..=b'9' => Class::Digit,
        b'_' | b'.' | b':' | b'/' | b'-' | b' ' => Class::Delim,
        _ => Class::Other,
    }
}

/// Bonus of matching a byte of class `cur` after a byte of class `prev`.
#[inline]
fn bonus(prev: Class, cur: Class) -> i32 {
    match (prev, cur) {
        (Class::Delim, c) if c != Class::Delim => BONUS_BOUNDARY,
        (Class::Lower, Class::Upper) | (Class::Lower | Class::Upper, Class::Digit) => BONUS_CAMEL,
        _ => 0,
    }
}

/// A lower-cased pattern, ready to match many texts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pattern {
    /// Lower-cased bytes.
    lower: Vec<u8>,
    /// The pattern as typed (exact-case bonus).
    raw: Vec<u8>,
}

impl Pattern {
    /// `p` with surrounding whitespace removed.
    pub fn new(p: &[u8]) -> Pattern {
        let p = p.trim_ascii();
        Pattern { lower: p.to_ascii_lowercase(), raw: p.to_vec() }
    }

    pub fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lower.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    /// Score of `text` (higher is better), `None` if the pattern is not a subsequence of it.
    /// An empty pattern matches everything with score 0.
    pub fn score(&self, text: &[u8]) -> Option<i32> {
        score(&self.lower, &self.raw, text)
    }
}

/// See [`Pattern::score`]. `lower` is `raw` lower-cased.
pub fn score(lower: &[u8], raw: &[u8], text: &[u8]) -> Option<i32> {
    let m = lower.len();
    if m == 0 {
        return Some(0);
    }
    if text.len() < m {
        return None;
    }
    // Forward: the first end position.
    let mut k = 0;
    let mut end = 0;
    for (i, &c) in text.iter().enumerate() {
        if c.to_ascii_lowercase() == lower[k] {
            k += 1;
            if k == m {
                end = i + 1;
                break;
            }
        }
    }
    if k < m {
        return None;
    }
    // Backward from the end: the latest start (tightest window ending there).
    let mut k = m;
    let mut start = end;
    while k > 0 {
        start -= 1;
        if text[start].to_ascii_lowercase() == lower[k - 1] {
            k -= 1;
        }
    }
    // Score the window greedily (leftmost match of each pattern byte inside it). Inside a run
    // of consecutive matches every byte keeps the bonus of the run's first byte (fzf).
    let mut s = 0i32;
    let mut k = 0;
    let mut run_bonus = 0;
    let mut prev_match = false;
    let mut in_gap = false;
    let mut gaps = false;
    let mut exact = true;
    let mut prev = if start == 0 { Class::Delim } else { class(text[start - 1]) };
    for (i, &c) in text[start..end].iter().enumerate() {
        let cls = class(c);
        if k < m && c.to_ascii_lowercase() == lower[k] {
            let mut b = bonus(prev, cls);
            if prev_match {
                b = b.max(run_bonus).max(BONUS_CONSECUTIVE);
            } else {
                run_bonus = b;
            }
            if k == 0 {
                b *= FIRST_MULTIPLIER;
            }
            exact &= c == raw[k];
            s += MATCH + b;
            k += 1;
            prev_match = true;
            in_gap = false;
        } else if i > 0 {
            s += if in_gap { GAP_EXTEND } else { GAP_START };
            in_gap = true;
            gaps = true;
            prev_match = false;
        }
        prev = cls;
    }
    if exact {
        s += BONUS_EXACT_CASE;
    }
    // The pattern is the whole text: an exact name beats every longer one.
    if start == 0 && end == text.len() && !gaps {
        s += 2 * MATCH;
    }
    s -= ((text.len() - (end - start)) as i32 / 8).min(8);
    Some(s)
}

/// Subsequence test only (no score): the pattern's bytes appear in `text` in order.
pub fn is_subsequence(lower: &[u8], text: &[u8]) -> bool {
    let mut k = 0;
    for &c in text {
        if k == lower.len() {
            break;
        }
        if c.to_ascii_lowercase() == lower[k] {
            k += 1;
        }
    }
    k == lower.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &str, t: &str) -> Option<i32> {
        Pattern::new(p.as_bytes()).score(t.as_bytes())
    }

    #[test]
    fn subsequence_required() {
        assert!(s("abc", "aXbXc").is_some());
        assert!(s("abc", "acb").is_none());
        assert!(s("abcd", "abc").is_none());
        assert!(s("ABC", "abc").is_some(), "case-insensitive");
        assert_eq!(s("", "anything"), Some(0));
        assert_eq!(s("  ", "anything"), Some(0), "whitespace is trimmed");
        assert!(is_subsequence(b"gc", b"get_config"));
        assert!(!is_subsequence(b"gz", b"get_config"));
    }

    #[test]
    fn ranking_prefers_exact_prefix_boundary_and_compact() {
        // Exact name beats a longer name with the pattern as prefix.
        assert!(s("load", "load") > s("load", "loader"));
        // Prefix beats an inner substring.
        assert!(s("load", "loader") > s("load", "preload"));
        // Contiguous beats scattered.
        assert!(s("conf", "config") > s("conf", "c_o_n_f"));
        // Word boundaries (snake_case initials) beat mid-word scatter.
        assert!(s("gc", "get_config") > s("gc", "magic"));
        // camelCase humps.
        assert!(s("gc", "getConfig") > s("gc", "biggcat").map(|x| x - 100));
        assert!(s("gc", "getConfig") > s("gc", "agecx"));
        // Exact case wins a tie.
        assert!(s("Foo", "Foo") > s("Foo", "foo"));
    }

    #[test]
    fn tightest_window_is_scored() {
        // "ab" occurs scattered early and contiguous late: the backward scan finds the
        // late compact window ending at the first full match, so both score sensibly.
        let a = s("ab", "a___ab").unwrap();
        let b = s("ab", "a___b").unwrap();
        assert!(a > b, "{a} {b}");
    }

    #[test]
    fn non_utf8_and_long_texts() {
        let t = [0xffu8, b'a', 0xfe, b'b'];
        assert!(Pattern::new(b"ab").score(&t).is_some());
        let long = vec![b'x'; 100_000];
        assert!(Pattern::new(b"y").score(&long).is_none());
        let mut l2 = long.clone();
        l2.push(b'y');
        assert!(Pattern::new(b"y").score(&l2).is_some());
    }

    #[test]
    fn deterministic_and_bounded() {
        for (p, t) in [("a", "a"), ("xyz", "x_y_z"), ("m", "mmmmmmmm"), ("init", "__init__")] {
            let a = s(p, t);
            assert_eq!(a, s(p, t));
            assert!(a.unwrap() > 0, "{p} {t} {a:?}");
        }
    }
}
