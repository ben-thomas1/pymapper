//! Own gitignore matcher: one parsed ignore file (`.gitignore`, `.git/info/exclude`, or a list
//! of configured excludes), matched against paths relative to the directory it applies to.
//!
//! Semantics follow git (`dir.c` + `wildmatch.c`):
//! - Blank lines and `#` comments are skipped; `\#` and `\!` escape a leading `#`/`!`.
//! - Trailing spaces are dropped unless escaped (`\ `); a trailing CR is dropped; a leading
//!   UTF-8 BOM is skipped.
//! - `!` negates (re-includes). The last matching pattern in a file wins.
//! - A trailing `/` makes the pattern match directories only.
//! - A pattern without any other `/` matches the basename at any depth. A pattern with a `/`
//!   at the start or in the middle is anchored to the ignore file's directory.
//! - `*`, `?`, `[...]`, `**` as in [`super::wildmatch`].
//!
//! Matching is case-sensitive (git's default `core.ignorecase=false`).
//!
//! Lookup cost: literal basename patterns (`build`, `node_modules/`) and suffix patterns
//! (`*.pyc`, `*.egg-info`) are found by binary search in sorted indexes, so a typical
//! template-sized file costs a few comparisons per path instead of one glob per pattern.
//! Only the remaining glob/anchored patterns are scanned, newest first, stopping as soon as a
//! pattern older than the best indexed hit is reached.

use std::cmp::Ordering;

use super::wildmatch::{GlobFlaw, glob_flaw, wildmatch};

/// Outcome of matching one path against an ignore file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// No pattern matched; ask the next (outer) ignore file.
    None,
    /// The last matching pattern ignores the path.
    Ignore,
    /// The last matching pattern is a negation (`!pat`): the path is explicitly kept.
    Whitelist,
}

const NEGATIVE: u8 = 1;
const MUST_BE_DIR: u8 = 2;
/// No `/` in the pattern: match against the basename only.
const NO_DIR: u8 = 4;
/// `*literal`: a plain suffix test on the basename.
const ENDS_WITH: u8 = 8;

#[derive(Clone, Copy, Debug)]
struct Pattern {
    /// Byte range of the pattern in `Gitignore::buf` (after `!`, leading `/` and trailing `/`
    /// are stripped).
    start: u32,
    len: u32,
    /// Length of the leading run without glob metacharacters.
    prefix: u32,
    flags: u8,
}

/// Index entry: a key (byte range in `buf`) and the pattern it belongs to.
#[derive(Clone, Copy, Debug)]
struct Key {
    start: u32,
    len: u32,
    pat: u32,
}

/// A parsed ignore file. Immutable after construction; cheap to share behind an `Arc`.
#[derive(Clone, Debug, Default)]
pub struct Gitignore {
    buf: Vec<u8>,
    pats: Vec<Pattern>,
    /// `NO_DIR` patterns without metacharacters, keyed by the whole pattern; sorted by key.
    literal: Vec<Key>,
    /// `NO_DIR | ENDS_WITH` patterns whose suffix contains a `.`, keyed by the bytes after the
    /// last `.` (the basename's extension); sorted by key.
    ext: Vec<Key>,
    /// All other patterns, in descending pattern order.
    rest: Vec<u32>,
}

impl Gitignore {
    /// Parses the contents of an ignore file. Never fails: like git, malformed patterns are
    /// kept and simply never match.
    pub fn parse(src: &[u8]) -> Gitignore {
        let mut gi = Gitignore::default();
        let src = src.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(src);
        for line in src.split(|&c| c == b'\n') {
            gi.add_line(line);
        }
        gi.build_index();
        gi
    }

    /// Builds a matcher from individual patterns (one gitignore line each), e.g. configured
    /// excludes.
    pub fn from_patterns<S: AsRef<[u8]>>(patterns: &[S]) -> Gitignore {
        let mut gi = Gitignore::default();
        for p in patterns {
            gi.add_line(p.as_ref());
        }
        gi.build_index();
        gi
    }

    /// Number of patterns kept (comments and blank lines excluded).
    pub fn len(&self) -> usize {
        self.pats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    /// Checks one configured pattern (a single gitignore line). Git accepts every line and
    /// silently never matches the broken ones; configured excludes are reported instead.
    pub fn check_pattern(line: &[u8]) -> Result<(), PatternError> {
        if line.first() == Some(&b'#') {
            return Err(PatternError::Comment);
        }
        let Some((pat, _)) = parse_line(line) else { return Err(PatternError::Empty) };
        match glob_flaw(pat) {
            None => Ok(()),
            Some(GlobFlaw::TrailingBackslash) => Err(PatternError::TrailingBackslash),
            Some(GlobFlaw::BadClass) => Err(PatternError::BadClass),
        }
    }

    fn add_line(&mut self, line: &[u8]) {
        if line.first() == Some(&b'#') {
            return;
        }
        let Some((line, mut flags)) = parse_line(line) else { return };
        let prefix = simple_length(line);
        if line[0] == b'*' && simple_length(&line[1..]) == line.len() - 1 {
            flags |= ENDS_WITH;
        }
        // Lines longer than 4 GiB do not exist in practice; skip rather than truncate.
        let (Ok(start), Ok(len), Ok(prefix)) =
            (u32::try_from(self.buf.len()), u32::try_from(line.len()), u32::try_from(prefix))
        else {
            return;
        };
        self.buf.extend_from_slice(line);
        self.pats.push(Pattern { start, len, prefix, flags });
    }

    fn build_index(&mut self) {
        for (i, p) in self.pats.iter().enumerate() {
            let i = i as u32;
            if p.flags & NO_DIR != 0 && p.prefix == p.len {
                self.literal.push(Key { start: p.start, len: p.len, pat: i });
            } else if p.flags & (NO_DIR | ENDS_WITH) == NO_DIR | ENDS_WITH {
                let suffix = &self.buf[p.start as usize + 1..(p.start + p.len) as usize];
                match suffix.iter().rposition(|&c| c == b'.') {
                    Some(dot) => {
                        let start = p.start + 1 + dot as u32 + 1;
                        self.ext.push(Key { start, len: p.start + p.len - start, pat: i });
                    }
                    None => self.rest.push(i),
                }
            } else {
                self.rest.push(i);
            }
        }
        self.rest.reverse();
        let buf = &self.buf;
        let key = |k: &Key| &buf[k.start as usize..(k.start + k.len) as usize];
        self.literal.sort_unstable_by(|a, b| key(a).cmp(key(b)).then(a.pat.cmp(&b.pat)));
        self.ext.sort_unstable_by(|a, b| key(a).cmp(key(b)).then(a.pat.cmp(&b.pat)));
    }

    #[inline]
    fn bytes(&self, start: u32, len: u32) -> &[u8] {
        &self.buf[start as usize..(start + len) as usize]
    }

    /// Matches `path`, relative to this file's directory (no leading `/`, components joined by
    /// `/`), against the patterns. `is_dir` tells whether the path is a directory.
    pub fn matched(&self, path: &[u8], is_dir: bool) -> Match {
        if self.pats.is_empty() {
            return Match::None;
        }
        let basename = match path.iter().rposition(|&c| c == b'/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        // Best (highest-numbered) matching pattern so far.
        let mut best: Option<u32> = None;
        for k in self.equal_range(&self.literal, basename).iter().rev() {
            // Sorted by pattern within equal keys: the first eligible from the back wins.
            if self.dir_ok(k.pat, is_dir) {
                best = Some(k.pat);
                break;
            }
        }
        if let Some(dot) = basename.iter().rposition(|&c| c == b'.') {
            let ext = &basename[dot + 1..];
            for k in self.equal_range(&self.ext, ext).iter().rev() {
                if best.is_some_and(|b| k.pat < b) {
                    break;
                }
                let p = self.pats[k.pat as usize];
                if self.dir_ok(k.pat, is_dir) && basename.ends_with(self.bytes(p.start + 1, p.len - 1)) {
                    best = Some(k.pat);
                    break;
                }
            }
        }
        for &i in &self.rest {
            if best.is_some_and(|b| i < b) {
                break;
            }
            if self.dir_ok(i, is_dir) && self.pattern_matches(i, path, basename) {
                best = Some(i);
                break;
            }
        }
        match best {
            None => Match::None,
            Some(i) if self.pats[i as usize].flags & NEGATIVE != 0 => Match::Whitelist,
            Some(_) => Match::Ignore,
        }
    }

    #[inline]
    fn dir_ok(&self, i: u32, is_dir: bool) -> bool {
        is_dir || self.pats[i as usize].flags & MUST_BE_DIR == 0
    }

    /// The run of `keys` whose key equals `needle`.
    fn equal_range<'a>(&self, keys: &'a [Key], needle: &[u8]) -> &'a [Key] {
        if keys.is_empty() {
            return keys;
        }
        let key = |k: &Key| self.bytes(k.start, k.len);
        let lo = keys.partition_point(|k| key(k) < needle);
        let hi = lo + keys[lo..].partition_point(|k| key(k).cmp(needle) == Ordering::Equal);
        &keys[lo..hi]
    }

    /// git's `match_basename` / `match_pathname` for one pattern.
    fn pattern_matches(&self, i: u32, path: &[u8], basename: &[u8]) -> bool {
        let p = self.pats[i as usize];
        let pat = self.bytes(p.start, p.len);
        if p.flags & NO_DIR != 0 {
            if p.prefix == p.len {
                return pat == basename;
            }
            if p.flags & ENDS_WITH != 0 {
                return basename.ends_with(&pat[1..]);
            }
            return wildmatch(pat, basename);
        }
        // Anchored: the literal prefix must match exactly, the rest goes to wildmatch.
        let prefix = p.prefix as usize;
        if prefix > 0 {
            if prefix > path.len() || pat[..prefix] != path[..prefix] {
                return false;
            }
            if prefix == pat.len() {
                return prefix == path.len();
            }
        }
        wildmatch(&pat[prefix..], &path[prefix..])
    }
}

/// Why a configured pattern is rejected ([`Gitignore::check_pattern`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// Starts with `#`, so gitignore syntax reads it as a comment.
    #[error("starts with `#`, which makes it a comment (write `\\#` for a literal `#`)")]
    Comment,
    /// Nothing left after `!`, a trailing `/` and spaces are stripped.
    #[error("matches nothing (empty pattern)")]
    Empty,
    /// Ends in a lone `\`.
    #[error("ends in a lone backslash and matches nothing")]
    TrailingBackslash,
    /// Unterminated `[...]` or unknown `[:name:]` class.
    #[error("has an unterminated `[` or an unknown `[:class:]` and matches nothing")]
    BadClass,
}

/// Normalises one line (CR, trailing spaces, `!`, trailing `/`, leading `/`) into the pattern
/// bytes and flags, or `None` for a line that can never match (blank, `!`, `/`, `!/`). The
/// caller has already dropped `#` comments.
fn parse_line(line: &[u8]) -> Option<(&[u8], u8)> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut line = trim_trailing_spaces(line);
    let mut flags = 0u8;
    if line.first() == Some(&b'!') {
        flags |= NEGATIVE;
        line = &line[1..];
    }
    if line.last() == Some(&b'/') {
        flags |= MUST_BE_DIR;
        line = &line[..line.len() - 1];
    }
    if line.is_empty() {
        return None;
    }
    if !line.contains(&b'/') {
        flags |= NO_DIR;
    } else if line[0] == b'/' {
        // Anchoring is implicit for patterns containing '/'; drop the leading one.
        line = &line[1..];
        if line.is_empty() {
            return None;
        }
    }
    Some((line, flags))
}

/// git's `trim_trailing_spaces`: drops trailing unescaped spaces (not tabs).
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                if last_space.is_none() {
                    last_space = Some(i);
                }
            }
            b'\\' => {
                i += 1;
                if i >= line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    match last_space {
        Some(end) => &line[..end],
        None => line,
    }
}

/// Length of the leading run of `s` without glob metacharacters (git's `simple_length`).
fn simple_length(s: &[u8]) -> usize {
    s.iter().position(|&c| matches!(c, b'*' | b'?' | b'[' | b'\\')).unwrap_or(s.len())
}

#[cfg(test)]
mod index_tests {
    use super::*;

    /// Reference: git's plain "last matching pattern wins" scan, without any index.
    fn naive(gi: &Gitignore, path: &[u8], is_dir: bool) -> Match {
        let basename = match path.iter().rposition(|&c| c == b'/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        for i in (0..gi.pats.len() as u32).rev() {
            if gi.dir_ok(i, is_dir) && gi.pattern_matches(i, path, basename) {
                return if gi.pats[i as usize].flags & NEGATIVE != 0 { Match::Whitelist } else { Match::Ignore };
            }
        }
        Match::None
    }

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    const NAMES: &[&str] = &["a", "b", "a.py", "b.py", "x.tar.gz", "x.gz", "c~", "build", "a.b.py", ".py"];
    const PATS: &[&str] = &[
        "a", "b", "a.py", "*.py", "*.gz", "*.tar.gz", "*~", "build", "build/", "*", "a*", "?.py", "/a",
        "a/b", "a/**", "**/b", "b/*.py", "[ab].py", "*.b.py", "x.*", "**", "a/**/b.py", "*/a.py",
    ];

    #[test]
    fn indexed_lookup_agrees_with_linear_scan() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..400 {
            let n = 1 + (rng.next() % 8) as usize;
            let mut lines = Vec::new();
            for _ in 0..n {
                let neg = if rng.next().is_multiple_of(3) { "!" } else { "" };
                lines.push(format!("{neg}{}", rng.pick(PATS)));
            }
            let gi = Gitignore::from_patterns(&lines);
            for _ in 0..40 {
                let depth = 1 + (rng.next() % 3) as usize;
                let mut path = String::new();
                for d in 0..depth {
                    if d > 0 {
                        path.push('/');
                    }
                    path.push_str(rng.pick(NAMES));
                }
                for is_dir in [false, true] {
                    assert_eq!(
                        gi.matched(path.as_bytes(), is_dir),
                        naive(&gi, path.as_bytes(), is_dir),
                        "patterns {lines:?} path {path:?} dir {is_dir}"
                    );
                }
            }
        }
    }

    #[test]
    fn index_buckets() {
        let gi = Gitignore::from_patterns(&["build", "*.pyc", "*.tar.gz", "*~", "/abs", "a?c", "x/"]);
        assert_eq!(gi.literal.len(), 2, "build and x/");
        assert_eq!(gi.ext.len(), 2, "*.pyc and *.tar.gz");
        assert_eq!(gi.rest.len(), 3, "*~, /abs, a?c");
        // Rest is newest first.
        assert!(gi.rest.windows(2).all(|w| w[0] > w[1]));
    }
}
