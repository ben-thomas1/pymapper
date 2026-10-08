//! Inline suppressions: `# pymapper: ignore[PM101, PM4]` comments (design: Front end, "Records
//! `# pymapper: ignore[CODE,...]` comments").
//!
//! The lexer finds the directives and hands over the raw code list (the bytes between `[` and
//! `]`, or nothing for a bare `# pymapper: ignore`) with byte offsets; extraction maps the
//! offset to a line and calls [`parse_codes`], so only `(line, selector)` pairs outlive the
//! source. The loader collects every file's entries into one [`Suppressions`] table.
//!
//! Semantics (as ruff's `noqa`): a directive suppresses findings reported on the line it is on.
//! Codes are rule selectors, so `PM4` suppresses every dead-code rule and `ALL` (or a bare
//! `ignore`) every rule. A finding is reported on its definition's `def` / `class` line, its
//! import's line, or line 1 for module-level findings.

use crate::config::RuleSelector;

use super::{FileId, Rule};

/// One suppression: findings on `line` whose rule matches `selector` are not reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SuppressionEntry {
    /// 1-based line of the directive.
    pub line: u32,
    /// Suppressed rules.
    pub selector: RuleSelector,
}

/// Why a code in a directive was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BadCodeKind {
    /// Not `ALL` or `PM` followed by up to three digits (e.g. `E501`, `pm101`), or an empty
    /// list (`ignore[]`).
    Malformed,
    /// Well-formed but selects no registered rule (e.g. `PM999`). It is still recorded.
    Unknown,
}

/// A rejected code; offsets are relative to the code-list text passed to [`parse_codes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BadCode {
    /// Byte offset of the code in the list.
    pub start: u32,
    /// Byte length of the code (0 for an empty list).
    pub len: u32,
    /// What is wrong.
    pub kind: BadCodeKind,
}

/// Parses one directive on `line`. `codes` is the text between the brackets, or `None` for a
/// bare `ignore` (suppresses every rule). Codes are comma-separated selectors; spaces and tabs
/// around them and empty entries (`PM101,`) are ignored. Valid selectors are appended to `out`;
/// malformed ones are reported to `bad` and dropped, unknown ones reported and kept.
pub fn parse_codes(line: u32, codes: Option<&[u8]>, out: &mut Vec<SuppressionEntry>, mut bad: impl FnMut(BadCode)) {
    let Some(list) = codes else {
        out.push(SuppressionEntry { line, selector: RuleSelector::All });
        return;
    };
    let before = out.len();
    let mut malformed = false;
    let mut off = 0usize;
    for part in list.split(|&b| b == b',') {
        let lead = part.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let code = trim_end(&part[lead..]);
        let start = (off + lead) as u32;
        off += part.len() + 1;
        if code.is_empty() {
            continue;
        }
        let len = code.len() as u32;
        match selector_from_bytes(code) {
            Some(selector) => {
                if !super::all().any(|d| selector.matches(d.code)) {
                    bad(BadCode { start, len, kind: BadCodeKind::Unknown });
                }
                out.push(SuppressionEntry { line, selector });
            }
            None => {
                malformed = true;
                bad(BadCode { start, len, kind: BadCodeKind::Malformed });
            }
        }
    }
    if out.len() == before && !malformed {
        // `ignore[]` (or only commas/blanks): almost certainly a mistake, and suppressing
        // everything would hide it.
        bad(BadCode { start: 0, len: 0, kind: BadCodeKind::Malformed });
    }
}

fn trim_end(mut s: &[u8]) -> &[u8] {
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// `ALL` or `PM` followed by zero to three digits (same syntax as config selectors).
fn selector_from_bytes(code: &[u8]) -> Option<RuleSelector> {
    if code == b"ALL" {
        return Some(RuleSelector::All);
    }
    let digits = code.strip_prefix(b"PM")?;
    if digits.len() > 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = digits.iter().fold(0u16, |acc, &b| acc * 10 + u16::from(b - b'0'));
    Some(RuleSelector::Prefix { value, digits: digits.len() as u8 })
}

/// Entries of one file: `entries[start..start + len]`, sorted by line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileSpan {
    file: FileId,
    start: u32,
    len: u32,
}

/// Collects per-file suppressions into a [`Suppressions`] table.
#[derive(Clone, Debug, Default)]
pub struct SuppressionsBuilder {
    files: Vec<FileSpan>,
    entries: Vec<SuppressionEntry>,
}

impl SuppressionsBuilder {
    /// An empty builder.
    pub fn new() -> SuppressionsBuilder {
        SuppressionsBuilder::default()
    }

    /// Adds a file's entries (any order). Files without entries need not be added; adding the
    /// same file twice merges the entries.
    pub fn push_file(&mut self, file: FileId, entries: &[SuppressionEntry]) {
        if entries.is_empty() {
            return;
        }
        let start = self.entries.len();
        self.entries.extend_from_slice(entries);
        self.entries[start..].sort_unstable_by_key(|e| e.line);
        let len = u32::try_from(entries.len()).expect("more than u32::MAX suppressions in one file");
        let start = u32::try_from(start).expect("more than u32::MAX suppressions");
        self.files.push(FileSpan { file, start, len });
    }

    /// Finishes the table. O(entries) when files were pushed in increasing id order (as the
    /// loader does); otherwise they are regrouped once.
    pub fn build(self) -> Suppressions {
        let SuppressionsBuilder { mut files, entries } = self;
        if files.windows(2).all(|w| w[0].file < w[1].file) {
            return Suppressions { files, entries };
        }
        files.sort_by_key(|s| s.file); // stable: keeps push order within a file
        let mut out_files: Vec<FileSpan> = Vec::with_capacity(files.len());
        let mut out_entries = Vec::with_capacity(entries.len());
        for span in &files {
            let s = span.start as usize;
            let chunk = &entries[s..s + span.len as usize];
            let start = out_entries.len() as u32;
            out_entries.extend_from_slice(chunk);
            match out_files.last_mut() {
                Some(last) if last.file == span.file => {
                    last.len += span.len;
                    let s = last.start as usize;
                    out_entries[s..].sort_by_key(|e| e.line);
                }
                _ => out_files.push(FileSpan { file: span.file, start, len: span.len }),
            }
        }
        Suppressions { files: out_files, entries: out_entries }
    }
}

/// Every inline suppression of the run. Memory is proportional to the number of directives
/// (files without any cost nothing); lookups are two binary searches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Suppressions {
    files: Vec<FileSpan>,
    entries: Vec<SuppressionEntry>,
}

impl Suppressions {
    /// No suppressions.
    pub fn new() -> Suppressions {
        Suppressions::default()
    }

    /// Total number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A file's entries, sorted by line.
    pub fn for_file(&self, file: FileId) -> &[SuppressionEntry] {
        match self.files.binary_search_by_key(&file, |s| s.file) {
            Ok(i) => {
                let s = self.files[i];
                &self.entries[s.start as usize..(s.start + s.len) as usize]
            }
            Err(_) => &[],
        }
    }

    /// Whether a finding of `rule` at `file:line` is suppressed.
    #[inline]
    pub fn is_suppressed(&self, file: FileId, line: u32, rule: Rule) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let entries = self.for_file(file);
        let first = entries.partition_point(|e| e.line < line);
        entries[first..].iter().take_while(|e| e.line == line).any(|e| e.selector.matches(rule.0))
    }
}
