//! `# pymapper: ignore[CODE,...]` suppression comments, recorded by the lexer.

/// A suppression directive found in a comment.
///
/// Recognised form: a `#` (the comment's own or a later one in the same comment,
/// e.g. `# noqa: E501  # pymapper: ignore[PM101]`), optional spaces/tabs,
/// `pymapper:`, optional spaces/tabs, `ignore`, then either `[CODES]` or the end
/// of the directive (end of comment, whitespace or `#`), which suppresses all
/// codes. Offsets are byte offsets into the source; map them to lines with the
/// caller's line index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Suppression {
    /// Offset of the `#` that starts the directive.
    pub start: u32,
    /// Offset of the text between `[` and `]` (meaningless when `all`).
    pub codes_start: u32,
    /// Length of the text between `[` and `]`.
    pub codes_len: u32,
    /// Bare `ignore` without a code list: suppresses every code on the line.
    pub all: bool,
}

impl Suppression {
    /// The listed codes, trimmed of spaces/tabs, empty entries skipped.
    pub fn codes(self, src: &[u8]) -> impl Iterator<Item = &[u8]> {
        let s = self.codes_start as usize;
        let list: &[u8] = if self.all { &[] } else { &src[s..s + self.codes_len as usize] };
        list.split(|&b| b == b',').map(trim).filter(|c| !c.is_empty())
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// Shortest possible directive: `#pymapper:ignore`.
const MIN_LEN: usize = 16;

/// Scans the comment `src[start..end]` (starting at its `#`) for directives.
pub(super) fn scan(src: &[u8], start: usize, end: usize, out: &mut Vec<Suppression>) {
    if end - start < MIN_LEN {
        return;
    }
    let c = &src[start..end];
    let mut i = 0;
    while let Some(off) = c[i..].iter().position(|&b| b == b'#') {
        let hash = i + off;
        i = hash + 1;
        if let Some(mut s) = parse(c, hash) {
            s.start += start as u32;
            s.codes_start += start as u32;
            out.push(s);
        }
    }
}

fn skip_ws(c: &[u8], mut j: usize) -> usize {
    while matches!(c.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    j
}

/// Parses a directive at `c[hash]`; offsets in the result are relative to `c`.
fn parse(c: &[u8], hash: usize) -> Option<Suppression> {
    let mut j = skip_ws(c, hash + 1);
    j += c[j..].starts_with(b"pymapper:").then_some(9)?;
    j = skip_ws(c, j);
    j += c[j..].starts_with(b"ignore").then_some(6)?;
    match c.get(j) {
        Some(b'[') => {
            let open = j + 1;
            let close = open + c[open..].iter().position(|&b| b == b']')?;
            Some(Suppression { start: hash as u32, codes_start: open as u32, codes_len: (close - open) as u32, all: false })
        }
        None | Some(b' ' | b'\t' | b'#') => Some(Suppression { start: hash as u32, codes_start: j as u32, codes_len: 0, all: true }),
        // `ignored`, `ignore-file`, `ignore:` ... are not directives.
        Some(_) => None,
    }
}
