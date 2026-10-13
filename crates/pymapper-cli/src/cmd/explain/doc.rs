//! The two renderings of `explain` (human and llm) behind one small document builder, and the
//! source lines it quotes (re-read from disk on demand: the index keeps no sources).
//!
//! ```text
//!   human                                   llm
//!   PM401-3fa2  dead code · ...             # pymapper explain PM401-3fa2
//!                                           PM401-3fa2 dead code · ...
//!   Source  services/pricing.py:53-54       ## Source services/pricing.py:53-54
//!      53 | def legacy_discount(p):         53 | def legacy_discount(p):
//!   Evidence                                ## Evidence
//!     callers: none                         callers: none
//! ```
//!
//! No colour in either; the llm form has no indentation (fewer tokens) and Markdown headings.

use std::collections::HashMap;
use std::path::Path;

use crate::cmd::check::RelPath;

/// Lines of source quoted at most per block (the rest is counted).
pub const MAX_QUOTE: usize = 40;

/// The output document.
pub struct Doc {
    pub llm: bool,
    pub out: String,
    /// Base of displayed paths (the working directory).
    cwd: std::path::PathBuf,
}

impl Doc {
    pub fn new(llm: bool, cwd: &Path) -> Doc {
        Doc { llm, out: String::with_capacity(4096), cwd: cwd.to_path_buf() }
    }

    /// `path` (absolute) relative to the working directory.
    pub fn rel(&self, path: &Path) -> String {
        RelPath(path, &self.cwd).to_string()
    }

    /// A top-level heading: llm `# text`, human the text.
    pub fn title(&mut self, text: &str) {
        if self.llm {
            self.out.push_str("# ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// A finding's first line: flush left in both forms, after a blank line unless first.
    pub fn head(&mut self, text: &str) {
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// A line directly under a head (location, confidence): indented in human output.
    pub fn sub(&mut self, text: &str) {
        if !self.llm {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// A section heading: llm `## name`, human `  name` after a blank line.
    pub fn section(&mut self, name: &str) {
        self.out.push('\n');
        self.out.push_str(if self.llm { "## " } else { "  " });
        self.out.push_str(name);
        self.out.push('\n');
    }

    /// One item of a section.
    pub fn line(&mut self, text: &str) {
        if !self.llm {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// Source lines `first..=last` of `src` (1-based), at most [`MAX_QUOTE`], as
    /// `  53 | code`; `mark` lines get a `>` instead of the bar.
    pub fn quote(&mut self, src: &Source, first: u32, last: u32, mark: &[u32]) {
        let last = last.min(src.len());
        if first == 0 || first > last {
            self.line("(source not available)");
            return;
        }
        let width = last.to_string().len();
        let shown_last = last.min(first + MAX_QUOTE as u32 - 1);
        for n in first..=shown_last {
            let bar = if mark.contains(&n) { '>' } else { '|' };
            let text = src.line(n);
            let line = format!("{n:>width$} {bar} {text}");
            self.line(line.trim_end());
        }
        if shown_last < last {
            self.line(&format!("... {} more lines", last - shown_last));
        }
    }

    /// `Next: text` after a blank line.
    pub fn next(&mut self, text: &str) {
        self.out.push_str("\nNext: ");
        self.out.push_str(text);
        self.out.push('\n');
    }
}

/// One file's text, split into lines (invalid UTF-8 replaced).
pub struct Source {
    text: String,
    /// Byte offset of the start of each line.
    starts: Vec<usize>,
}

impl Source {
    fn new(bytes: &[u8]) -> Source {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        if starts.last() == Some(&text.len()) && starts.len() > 1 {
            starts.pop();
        }
        Source { text, starts }
    }

    /// Number of lines.
    pub fn len(&self) -> u32 {
        if self.text.is_empty() { 0 } else { self.starts.len() as u32 }
    }

    /// Line `n` (1-based) without its line break; empty when out of range.
    pub fn line(&self, n: u32) -> &str {
        let i = n as usize;
        if i == 0 || i > self.starts.len() {
            return "";
        }
        let start = self.starts[i - 1];
        let end = self.starts.get(i).copied().unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// Sources read so far, by path (each file is read at most once per run).
#[derive(Default)]
pub struct Sources {
    files: HashMap<std::path::PathBuf, Option<Source>>,
}

impl Sources {
    /// The source of `path`, `None` when it cannot be read (deleted since the analysis).
    pub fn get(&mut self, path: &Path) -> Option<&Source> {
        self.files
            .entry(path.to_path_buf())
            .or_insert_with(|| std::fs::read(path).ok().map(|b| Source::new(&b)))
            .as_ref()
    }
}

/// Whether `hay[at..at + len]` is a whole word (identifier boundaries on both sides).
fn whole_word(hay: &[u8], at: usize, len: usize) -> bool {
    let ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    (at == 0 || !ident(hay[at - 1])) && hay.get(at + len).is_none_or(|&b| !ident(b))
}

/// Lines (1-based) of `src` where `word` occurs as a whole word.
pub fn word_lines(src: &Source, word: &str) -> Vec<u32> {
    let mut out = Vec::new();
    if word.is_empty() {
        return out;
    }
    for n in 1..=src.len() {
        let line = src.line(n).as_bytes();
        let mut from = 0;
        while let Some(i) = find(&line[from..], word.as_bytes()) {
            let at = from + i;
            if whole_word(line, at, word.len()) {
                out.push(n);
                break;
            }
            from = at + 1;
        }
    }
    out
}

/// First occurrence of `needle` in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}
