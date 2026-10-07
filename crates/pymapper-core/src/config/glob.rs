//! Name globs for entrypoint rules: `*` matches any run of bytes (dots included), `?` exactly
//! one byte, everything else is literal. Matching is over bytes, so non-UTF-8 identifiers work.
//!
//! [`GlobSet`] buckets patterns by shape so the common cases are binary searches instead of a
//! scan over every pattern: exact names, `*.name` (last dotted segment), `prefix*`, `*suffix`,
//! and a small general remainder.

use std::fmt;

/// Error for a pattern that is not a valid name glob.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GlobError {
    /// The pattern is empty.
    #[error("empty pattern")]
    Empty,
    /// The pattern contains whitespace or an unsupported metacharacter.
    #[error("unsupported character {0:?} in pattern (only `*` and `?` are wildcards)")]
    BadChar(char),
}

/// One validated name glob.
#[derive(Clone, PartialEq, Eq)]
pub struct Glob {
    pat: Box<[u8]>,
}

impl fmt::Debug for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Glob({:?})", String::from_utf8_lossy(&self.pat))
    }
}

fn is_wild(b: u8) -> bool {
    b == b'*' || b == b'?'
}

impl Glob {
    /// Validates `pattern`.
    pub fn new(pattern: &str) -> Result<Glob, GlobError> {
        if pattern.is_empty() {
            return Err(GlobError::Empty);
        }
        if let Some(c) = pattern.chars().find(|&c| c.is_whitespace() || matches!(c, '[' | ']' | '{' | '}' | '\\')) {
            return Err(GlobError::BadChar(c));
        }
        Ok(Glob { pat: pattern.as_bytes().into() })
    }

    /// The pattern text.
    pub fn as_bytes(&self) -> &[u8] {
        &self.pat
    }

    /// Whether `text` matches the whole pattern.
    pub fn is_match(&self, text: &[u8]) -> bool {
        wildcard_match(&self.pat, text)
    }
}

/// Iterative `*`/`?` matcher: backtracks only to the most recent `*`, so it is O(len(p) *
/// len(t)) worst case and linear for typical patterns, with no allocation or recursion.
fn wildcard_match(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    // Position after the last `*` seen, and the text position it is currently matched up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) && p[pi] != b'*' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            pi += 1;
            star = Some((pi, ti));
        } else if let Some((sp, st)) = star {
            // Let the last `*` absorb one more byte and retry.
            pi = sp;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&b| b == b'*')
}

/// A set of name globs, compiled for fast "does any match" queries.
#[derive(Clone, Debug, Default)]
pub struct GlobSet {
    /// Wildcard-free patterns, sorted and deduplicated.
    exact: Vec<Box<[u8]>>,
    /// `X` for patterns `*.X` where `X` has no wildcard and no dot, sorted and deduplicated:
    /// matches text containing a dot whose last segment is `X`.
    last_segment: Vec<Box<[u8]>>,
    /// `X` for patterns `X*` (`X` wildcard-free).
    prefix: Vec<Box<[u8]>>,
    /// `X` for patterns `*X` (`X` wildcard-free) not covered by `last_segment`.
    suffix: Vec<Box<[u8]>>,
    /// Everything else.
    general: Vec<Glob>,
}

impl GlobSet {
    /// Compiles `patterns`; fails on the first invalid one.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<GlobSet, (usize, GlobError)> {
        let mut set = GlobSet::default();
        for (i, p) in patterns.iter().enumerate() {
            let glob = Glob::new(p.as_ref()).map_err(|e| (i, e))?;
            set.add(glob);
        }
        for v in [&mut set.exact, &mut set.last_segment, &mut set.prefix, &mut set.suffix] {
            v.sort_unstable();
            v.dedup();
        }
        set.general.dedup();
        Ok(set)
    }

    fn add(&mut self, glob: Glob) {
        let p = &glob.pat;
        let first_wild = p.iter().position(|&b| is_wild(b));
        let Some(first) = first_wild else {
            self.exact.push(glob.pat);
            return;
        };
        let wild_count = p.iter().filter(|&&b| is_wild(b)).count();
        if wild_count == 1 && p[first] == b'*' {
            if first == 0 {
                let rest = &p[1..];
                match rest.split_first() {
                    Some((b'.', seg)) if !seg.is_empty() && !seg.contains(&b'.') => self.last_segment.push(seg.into()),
                    _ => self.suffix.push(rest.into()),
                }
                return;
            }
            if first == p.len() - 1 {
                self.prefix.push(p[..first].into());
                return;
            }
        }
        self.general.push(glob);
    }

    /// Whether no pattern was added.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
            && self.last_segment.is_empty()
            && self.prefix.is_empty()
            && self.suffix.is_empty()
            && self.general.is_empty()
    }

    /// Whether any pattern matches `text`.
    pub fn is_match(&self, text: &[u8]) -> bool {
        if !self.exact.is_empty() && self.exact.binary_search_by(|e| (**e).cmp(text)).is_ok() {
            return true;
        }
        if !self.last_segment.is_empty()
            && let Some(dot) = text.iter().rposition(|&b| b == b'.')
        {
            let seg = &text[dot + 1..];
            if self.last_segment.binary_search_by(|e| (**e).cmp(seg)).is_ok() {
                return true;
            }
        }
        // The remaining buckets are tiny in practice (a handful of patterns per preset).
        self.prefix.iter().any(|x| text.starts_with(x))
            || self.suffix.iter().any(|x| text.ends_with(x))
            || self.general.iter().any(|g| g.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Glob::new(p).unwrap().is_match(t.as_bytes())
    }

    #[test]
    fn wildcard_semantics() {
        assert!(m("abc", "abc"));
        assert!(!m("abc", "abcd"));
        assert!(m("*", ""));
        assert!(m("*", "a.b.c"));
        assert!(m("a*", "a"));
        assert!(m("a*c", "abbbc"));
        assert!(!m("a*c", "abbb"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("*.route", "app.route"));
        assert!(m("*.route", "a.b.route"));
        assert!(!m("*.route", "route"));
        assert!(!m("*.route", "app.router"));
        assert!(m("test_*", "test_x"));
        assert!(m("*a*b*", "xxaxxbxx"));
        assert!(!m("*a*b*", "xxbxxaxx"));
        assert!(m("**", "anything"));
        assert!(m("a*b*c", "abcbc"));
        assert!(m("*?", "x"));
        assert!(!m("*?", ""));
    }

    #[test]
    fn rejects_bad_patterns() {
        assert_eq!(Glob::new(""), Err(GlobError::Empty));
        assert_eq!(Glob::new("a b"), Err(GlobError::BadChar(' ')));
        assert_eq!(Glob::new("a[bc]"), Err(GlobError::BadChar('[')));
        assert_eq!(GlobSet::new(&["ok", "{x}"]).unwrap_err(), (1, GlobError::BadChar('{')));
    }

    #[test]
    fn non_utf8_text() {
        let g = Glob::new("caf*").unwrap();
        assert!(g.is_match(b"caf\xe9"));
        let set = GlobSet::new(&["*.x"]).unwrap();
        assert!(set.is_match(b"\xff.x"));
    }

    /// Every bucket must agree with the plain matcher.
    #[test]
    fn set_agrees_with_globs() {
        let pats = [
            "exact", "*.route", "*.cli.command", "test_*", "*_handler", "a?c", "*mid*", "x*y", "*.", "*", "pre*",
        ];
        let texts = [
            "", "exact", "exac", "app.route", "route", "a.b.route", "app.router", "grp.cli.command", "cli.command",
            "test_", "test_foo", "my_handler", "abc", "axc", "ac", "amidb", "xy", "x..y", "a.", "prefix", ".route",
        ];
        for pat in pats {
            let set = GlobSet::new(&[pat]).unwrap();
            let glob = Glob::new(pat).unwrap();
            for t in texts {
                assert_eq!(set.is_match(t.as_bytes()), glob.is_match(t.as_bytes()), "pattern {pat:?} text {t:?}");
            }
        }
        let all = GlobSet::new(&pats[..pats.len() - 2]).unwrap();
        for t in texts {
            let expect = pats[..pats.len() - 2].iter().any(|p| m(p, t));
            assert_eq!(all.is_match(t.as_bytes()), expect, "text {t:?}");
        }
        assert!(GlobSet::new::<&str>(&[]).unwrap().is_empty());
        assert!(!GlobSet::new::<&str>(&[]).unwrap().is_match(b"x"));
    }
}
