//! "Did you mean" suggestions: optimal-string-alignment edit distance over bytes.

/// The candidate closest to `input`, if it is close enough to be a plausible typo.
///
/// `snake_case` spellings of `kebab-case` candidates always match. Otherwise the allowed
/// distance is a third of the input length (at least 1); ties keep the first candidate.
pub(crate) fn closest<'c>(input: &str, candidates: impl IntoIterator<Item = &'c str>) -> Option<&'c str> {
    let limit = (input.len() / 3).max(1);
    let mut best: Option<(usize, &'c str)> = None;
    for cand in candidates {
        if same_modulo_separator(input, cand) {
            return Some(cand);
        }
        let d = distance(input.as_bytes(), cand.as_bytes());
        if d <= limit && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, c)| c)
}

fn same_modulo_separator(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes().zip(b.bytes()).all(|(x, y)| {
            let norm = |c: u8| if c == b'_' { b'-' } else { c.to_ascii_lowercase() };
            norm(x) == norm(y)
        })
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions).
pub(crate) fn distance(a: &[u8], b: &[u8]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let w = b.len() + 1;
    // Three rolling rows: i-2, i-1, i.
    let mut rows = vec![0usize; 3 * w];
    for (j, cell) in rows[w..2 * w].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        let (prev2, rest) = rows.split_at_mut(w);
        let (prev, cur) = rest.split_at_mut(w);
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
        }
        // Shift rows: prev2 <- prev, prev <- cur.
        rows.copy_within(w..3 * w, 0);
    }
    rows[2 * w - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(distance(b"", b"abc"), 3);
        assert_eq!(distance(b"abc", b""), 3);
        assert_eq!(distance(b"select", b"select"), 0);
        assert_eq!(distance(b"selct", b"select"), 1);
        assert_eq!(distance(b"slecet", b"select"), 2);
        assert_eq!(distance(b"ab", b"ba"), 1);
        assert_eq!(distance(b"kitten", b"sitting"), 3);
    }

    #[test]
    fn suggestions() {
        let keys = ["select", "extend-select", "ignore", "per-file-ignores"];
        assert_eq!(closest("selct", keys), Some("select"));
        assert_eq!(closest("ignroe", keys), Some("ignore"));
        assert_eq!(closest("extend_select", keys), Some("extend-select"));
        assert_eq!(closest("PER_FILE_IGNORES", keys), Some("per-file-ignores"));
        assert_eq!(closest("banana", keys), None);
        assert_eq!(closest("x", keys), None);
    }
}
