//! Suppression comments recorded by the lexer: `# pymapper: ignore[CODE,...]` and ruff-style
//! `# noqa[: CODE ...]`.

/// A suppression directive found in a comment.
///
/// Recognised forms, each starting at a `#` (the comment's own or a later one in the same
/// comment, e.g. `# type: ignore  # noqa: F401`) followed by optional spaces/tabs:
///
/// - `pymapper:`, optional spaces/tabs, `ignore`, then either `[CODES]` (comma-separated) or
///   the end of the directive (end of comment, whitespace or `#`), which suppresses all codes.
/// - `noqa` (any case, as ruff), then either `:` (optionally surrounded by spaces/tabs) and a
///   list of codes (`[A-Za-z]+[0-9]+`, separated by commas and/or whitespace; the list ends at
///   the first word that is not a code), or the end of the directive (bare `noqa`). `noqa:`
///   without any code is not a directive (ruff warns about it and ignores it).
///
/// Offsets are byte offsets into the source; map them to lines with the caller's line index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Suppression {
    /// Offset of the `#` that starts the directive.
    pub start: u32,
    /// Offset of the code list (meaningless when `all`).
    pub codes_start: u32,
    /// Length of the code list.
    pub codes_len: u32,
    /// No code list (bare `ignore` / bare `noqa`): suppresses every code on the line.
    pub all: bool,
    /// A ruff-style `noqa` directive (its codes are ruff codes, not pymapper rules).
    pub noqa: bool,
}

impl Suppression {
    /// The listed codes, trimmed of spaces/tabs, empty entries skipped. `noqa` codes are also
    /// separated by whitespace.
    pub fn codes(self, src: &[u8]) -> impl Iterator<Item = &[u8]> {
        let s = self.codes_start as usize;
        let list: &[u8] = if self.all { &[] } else { &src[s..s + self.codes_len as usize] };
        let noqa = self.noqa;
        list.split(move |&b| b == b',' || (noqa && matches!(b, b' ' | b'\t'))).map(trim).filter(|c| !c.is_empty())
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

/// Shortest possible directive: `#noqa`.
const MIN_LEN: usize = 5;

/// Scans the comment `src[start..end]` (starting at its `#`) for directives.
pub(crate) fn scan(src: &[u8], start: usize, end: usize, out: &mut Vec<Suppression>) {
    if end - start < MIN_LEN {
        return;
    }
    let c = &src[start..end];
    let mut i = 0;
    while let Some(hash) = super::scan::find2(c, i, b'#', b'#') {
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
    if c.len() >= j + 4 && c[j..j + 4].eq_ignore_ascii_case(b"noqa") {
        return parse_noqa(c, hash, j + 4);
    }
    j += c[j..].starts_with(b"pymapper:").then_some(9)?;
    j = skip_ws(c, j);
    j += c[j..].starts_with(b"ignore").then_some(6)?;
    let start = hash as u32;
    match c.get(j) {
        Some(b'[') => {
            let open = j + 1;
            let close = open + c[open..].iter().position(|&b| b == b']')?;
            Some(Suppression { start, codes_start: open as u32, codes_len: (close - open) as u32, all: false, noqa: false })
        }
        None | Some(b' ' | b'\t' | b'#') => Some(Suppression { start, codes_start: j as u32, codes_len: 0, all: true, noqa: false }),
        // `ignored`, `ignore-file`, `ignore:` ... are not directives.
        Some(_) => None,
    }
}

/// Parses the rest of a `noqa` directive; `j` is just past `noqa`.
fn parse_noqa(c: &[u8], hash: usize, j: usize) -> Option<Suppression> {
    let start = hash as u32;
    let k = skip_ws(c, j);
    if c.get(k) != Some(&b':') {
        // Bare `noqa`, unless the word goes on (`noqanything`).
        return match c.get(j) {
            None | Some(b' ' | b'\t' | b'#') => Some(Suppression { start, codes_start: j as u32, codes_len: 0, all: true, noqa: true }),
            Some(_) => None,
        };
    }
    // Codes: words of letters then digits, separated by commas and/or spaces/tabs.
    let first = skip_ws(c, k + 1);
    let mut p = first;
    let mut end = first;
    loop {
        let w = code_len(&c[p..]);
        if w == 0 {
            break;
        }
        end = p + w;
        p = end;
        while matches!(c.get(p), Some(b' ' | b'\t' | b',')) {
            p += 1;
        }
    }
    (end > first).then_some(Suppression { start, codes_start: first as u32, codes_len: (end - first) as u32, all: false, noqa: true })
}

/// Length of the ruff code (`[A-Za-z]+[0-9]+`) at the start of `s`, ending at a separator or
/// the end of the comment; 0 if there is none.
fn code_len(s: &[u8]) -> usize {
    let letters = s.iter().take_while(|b| b.is_ascii_alphabetic()).count();
    let digits = s[letters..].iter().take_while(|b| b.is_ascii_digit()).count();
    let n = letters + digits;
    let ends = matches!(s.get(n), None | Some(b' ' | b'\t' | b',' | b'#'));
    if letters == 0 || digits == 0 || !ends { 0 } else { n }
}
