//! Git's `wildmatch` glob semantics over raw bytes (always in `WM_PATHNAME` mode).
//!
//! - `*` matches any run of bytes except `/`; `?` matches one byte except `/`.
//! - `**` matches across `/` only when it forms a whole path segment (`**/x`, `x/**`,
//!   `x/**/y`, or the whole pattern); otherwise it behaves like `*`.
//! - `[...]` classes with ranges, `!`/`^` negation, backslash escapes and the POSIX classes
//!   `[:alnum:]` etc. A class never matches `/`.
//! - `\x` matches `x` literally. A trailing lone backslash never matches.
//!
//! The control flow follows git's `dowild` including its two abort codes, which stop outer
//! `*` loops from retrying positions that cannot succeed. That keeps matching polynomial
//! (no exponential backtracking on patterns like `*a*a*a*b`).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum M {
    Match,
    NoMatch,
    /// The text ran out: no outer star can help by consuming more.
    AbortAll,
    /// A single `*` hit a `/`: only an outer `**` can help.
    AbortToStarStar,
}

/// Whether `text` matches the glob `pat` with gitignore (`WM_PATHNAME`) semantics.
#[inline]
pub(crate) fn wildmatch(pat: &[u8], text: &[u8]) -> bool {
    dowild(pat, text) == M::Match
}

#[inline]
fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// Byte at `i`, or 0 past the end (git works on NUL-terminated strings; gitignore lines
/// cannot contain NUL, so 0 is an unambiguous sentinel).
#[inline]
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn dowild(pat: &[u8], text: &[u8]) -> M {
    let mut p = 0usize;
    let mut t = 0usize;
    while p < pat.len() {
        let p_ch = pat[p];
        if t >= text.len() && p_ch != b'*' {
            return M::AbortAll;
        }
        let t_ch = at(text, t);
        match p_ch {
            b'\\' => {
                // Literal match with the next byte; a trailing backslash never matches.
                p += 1;
                if p >= pat.len() || t_ch != pat[p] {
                    return M::NoMatch;
                }
            }
            b'?' => {
                if t_ch == b'/' {
                    return M::NoMatch;
                }
            }
            b'*' => {
                let first = p;
                p += 1;
                let match_slash;
                if at(pat, p) == b'*' {
                    while at(pat, p) == b'*' {
                        p += 1;
                    }
                    let seg_start = first == 0 || pat[first - 1] == b'/';
                    let seg_end = p == pat.len()
                        || pat[p] == b'/'
                        || (pat[p] == b'\\' && at(pat, p + 1) == b'/');
                    if seg_start && seg_end {
                        // `**/` may match zero directories: try the rest right here.
                        if at(pat, p) == b'/' && dowild(&pat[p + 1..], &text[t..]) == M::Match {
                            return M::Match;
                        }
                        match_slash = true;
                    } else {
                        match_slash = false;
                    }
                } else {
                    match_slash = false;
                }
                if p == pat.len() {
                    // Trailing `**` matches everything, trailing `*` only up to the next `/`.
                    if !match_slash && text[t..].contains(&b'/') {
                        return M::NoMatch;
                    }
                    return M::Match;
                }
                if !match_slash && pat[p] == b'/' {
                    // A single `*` followed by `/` consumes exactly one path segment.
                    let Some(off) = text[t..].iter().position(|&c| c == b'/') else {
                        // No `/` left, and an outer star consuming more cannot create one.
                        return M::AbortAll;
                    };
                    t += off + 1;
                    p += 1;
                    continue;
                }
                return star_loop(&pat[p..], text, t, match_slash);
            }
            b'[' => match class(pat, p, t_ch) {
                Some((end, true)) => p = end,
                Some((_, false)) => return M::NoMatch,
                None => return M::AbortAll,
            },
            _ => {
                if t_ch != p_ch {
                    return M::NoMatch;
                }
            }
        }
        p += 1;
        t += 1;
    }
    if t < text.len() { M::NoMatch } else { M::Match }
}

/// Tries every split point for a `*` (or `**` when `match_slash`) followed by `rest`.
fn star_loop(rest: &[u8], text: &[u8], mut t: usize, match_slash: bool) -> M {
    loop {
        if t >= text.len() {
            break;
        }
        // Fast skip: when the star is followed by a literal byte, only positions holding that
        // byte can start the rest of the match.
        if !is_glob_special(rest[0]) {
            let lit = rest[0];
            while t < text.len() && (match_slash || text[t] != b'/') {
                if text[t] == lit {
                    break;
                }
                t += 1;
            }
            if t >= text.len() {
                // The literal does not occur in the rest of the text: no outer star can help
                // by consuming more (this is what keeps `*a*a*a*b` polynomial).
                return M::AbortAll;
            }
            if text[t] != lit {
                // Stopped at a `/` that a single `*` cannot cross: only an outer `**` can help.
                return M::AbortToStarStar;
            }
        }
        let m = dowild(rest, &text[t..]);
        if m != M::NoMatch {
            if !match_slash || m != M::AbortToStarStar {
                return m;
            }
        } else if !match_slash && text[t] == b'/' {
            return M::AbortToStarStar;
        }
        t += 1;
    }
    M::AbortAll
}

/// Matches the bracket expression starting at `pat[start] == '['` against `t_ch`. Returns the
/// index of the closing `]` and whether `t_ch` is a member, or `None` for a malformed class
/// (no closing `]`, dangling backslash, unknown `[:name:]`), which aborts the whole match.
pub(super) fn class(pat: &[u8], start: usize, t_ch: u8) -> Option<(usize, bool)> {
    let mut p = start + 1;
    let mut p_ch = at(pat, p);
    if p_ch == b'^' {
        p_ch = b'!';
    }
    let negated = p_ch == b'!';
    if negated {
        p += 1;
        p_ch = at(pat, p);
    }
    let mut prev_ch = 0u8;
    let mut matched = false;
    loop {
        if p_ch == 0 {
            return None;
        }
        if p_ch == b'\\' {
            p += 1;
            p_ch = at(pat, p);
            if p_ch == 0 {
                return None;
            }
            if t_ch == p_ch {
                matched = true;
            }
        } else if p_ch == b'-' && prev_ch != 0 && at(pat, p + 1) != 0 && at(pat, p + 1) != b']' {
            p += 1;
            p_ch = pat[p];
            if p_ch == b'\\' {
                p += 1;
                p_ch = at(pat, p);
                if p_ch == 0 {
                    return None;
                }
            }
            if prev_ch <= t_ch && t_ch <= p_ch {
                matched = true;
            }
            // A range end cannot start another range.
            p_ch = 0;
        } else if p_ch == b'[' && at(pat, p + 1) == b':' {
            let s = p + 2;
            let mut q = s;
            while q < pat.len() && pat[q] != b']' {
                q += 1;
            }
            if q >= pat.len() {
                return None;
            }
            if q == s || pat[q - 1] != b':' {
                // No closing ":]": the '[' is an ordinary member.
                p_ch = b'[';
                if t_ch == p_ch {
                    matched = true;
                }
            } else {
                match posix_class(&pat[s..q - 1], t_ch) {
                    Some(true) => matched = true,
                    Some(false) => {}
                    None => return None,
                }
                p = q;
                p_ch = 0;
            }
        } else if t_ch == p_ch {
            matched = true;
        }
        prev_ch = p_ch;
        p += 1;
        p_ch = at(pat, p);
        if p_ch == b']' {
            break;
        }
    }
    Some((p, matched != negated && t_ch != b'/'))
}

/// `[:name:]` membership (C locale); `None` for an unknown class name.
fn posix_class(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        b"space" => c.is_ascii_whitespace() || c == 0x0b,
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

/// Why a glob can never match anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlobFlaw {
    /// A `\` with nothing after it.
    TrailingBackslash,
    /// A `[` without a closing `]`, or an unknown `[:name:]` class.
    BadClass,
}

/// Finds structural flaws that make [`wildmatch`] fail on every text (git keeps such
/// patterns silently; configured excludes report them instead).
pub(crate) fn glob_flaw(pat: &[u8]) -> Option<GlobFlaw> {
    let mut p = 0usize;
    while p < pat.len() {
        match pat[p] {
            b'\\' => {
                if p + 1 >= pat.len() {
                    return Some(GlobFlaw::TrailingBackslash);
                }
                p += 1;
            }
            b'[' => match class(pat, p, b'a') {
                Some((end, _)) => p = end,
                None => return Some(GlobFlaw::BadClass),
            },
            _ => {}
        }
        p += 1;
    }
    None
}
