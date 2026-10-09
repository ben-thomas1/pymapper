//! Aggregated results of a `diff-ruff` run and their text rendering.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use super::diff::{Category, Sink};
use super::facts::{FileFacts, SkipKind};

/// One kept example: `path:line: message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Example {
    pub path: String,
    pub line: u32,
    pub msg: String,
}

/// Fact totals for one side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub defs: u64,
    pub imports: u64,
    pub calls: u64,
}

impl Totals {
    pub fn add(&mut self, f: &FileFacts) {
        self.files += 1;
        self.defs += f.defs.len() as u64;
        self.imports += f.imports.len() as u64;
        self.calls += f.calls.len() as u64;
    }

    fn merge(&mut self, o: &Totals) {
        self.files += o.files;
        self.defs += o.defs;
        self.imports += o.imports;
        self.calls += o.calls;
    }
}

/// Per-category counts plus the first `max_examples` examples of each, in input order.
/// Built per worker and merged left-to-right, so "first" means first in sorted path order.
#[derive(Debug)]
pub struct Report {
    pub max_examples: usize,
    pub files_seen: u64,
    pub bytes: u64,
    pub oracle: Totals,
    pub candidate: Totals,
    pub counts: [u64; Category::COUNT],
    pub examples: [Vec<Example>; Category::COUNT],
    /// Files the oracle could not parse (excluded from comparison), by [`SkipKind`].
    pub oracle_skips: [u64; SkipKind::COUNT],
    pub oracle_skip_examples: Vec<Example>,
    pub read_errors: u64,
    pub read_error_examples: Vec<Example>,
}

impl Report {
    pub fn new(max_examples: usize) -> Self {
        Report {
            max_examples,
            files_seen: 0,
            bytes: 0,
            oracle: Totals::default(),
            candidate: Totals::default(),
            counts: [0; Category::COUNT],
            examples: std::array::from_fn(|_| Vec::new()),
            oracle_skips: [0; SkipKind::COUNT],
            oracle_skip_examples: Vec::new(),
            read_errors: 0,
            read_error_examples: Vec::new(),
        }
    }

    pub fn mismatches(&self) -> u64 {
        self.counts.iter().sum()
    }

    pub fn oracle_skipped(&self) -> u64 {
        self.oracle_skips.iter().sum()
    }

    fn keep(list: &mut Vec<Example>, max: usize, path: &Path, line: u32, msg: fmt::Arguments<'_>) {
        if list.len() < max {
            let mut s = String::new();
            let _ = s.write_fmt(msg);
            list.push(Example { path: path.display().to_string(), line, msg: s });
        }
    }

    pub fn oracle_skip(&mut self, path: &Path, kind: SkipKind, line: u32, msg: &str) {
        self.oracle_skips[kind as usize] += 1;
        Self::keep(&mut self.oracle_skip_examples, self.max_examples, path, line, format_args!("{}: {msg}", kind.as_str()));
    }

    pub fn read_error(&mut self, path: &Path, err: &dyn fmt::Display) {
        self.read_errors += 1;
        Self::keep(&mut self.read_error_examples, self.max_examples, path, 0, format_args!("{err}"));
    }

    /// A [`Sink`] attributing mismatches to `path`.
    pub fn sink<'a>(&'a mut self, path: &'a Path) -> FileSink<'a> {
        FileSink { report: self, path }
    }

    /// Appends `right` (which covers files after `self`'s) into `self`.
    pub fn merge(mut self, right: Report) -> Report {
        let max = self.max_examples;
        self.files_seen += right.files_seen;
        self.bytes += right.bytes;
        self.oracle.merge(&right.oracle);
        self.candidate.merge(&right.candidate);
        self.read_errors += right.read_errors;
        for (a, b) in self.counts.iter_mut().zip(right.counts) {
            *a += b;
        }
        for (a, b) in self.oracle_skips.iter_mut().zip(right.oracle_skips) {
            *a += b;
        }
        let lists = self.examples.iter_mut().zip(right.examples).chain([
            (&mut self.oracle_skip_examples, right.oracle_skip_examples),
            (&mut self.read_error_examples, right.read_error_examples),
        ]);
        for (a, b) in lists {
            let room = max.saturating_sub(a.len());
            a.extend(b.into_iter().take(room));
        }
        self
    }

    /// Renders the human-readable report. `candidate` is the candidate's name, or `None` when
    /// only the oracle ran.
    pub fn write(&self, out: &mut dyn Write, root: &str, candidate: Option<&str>, walk: Duration, work: Duration) -> io::Result<()> {
        let secs = work.as_secs_f64();
        writeln!(out, "diff-ruff: {root}")?;
        write!(out, "files: {} seen, {} parsed by ruff", self.files_seen, self.oracle.files)?;
        if self.oracle_skipped() > 0 {
            write!(out, ", {} skipped by ruff (", self.oracle_skipped())?;
            let mut first = true;
            for k in [SkipKind::Syntax, SkipKind::NotUtf8] {
                let n = self.oracle_skips[k as usize];
                if n > 0 {
                    write!(out, "{}{} {n}", if first { "" } else { ", " }, k.as_str())?;
                    first = false;
                }
            }
            write!(out, ")")?;
        }
        if self.read_errors > 0 {
            write!(out, ", {} unreadable", self.read_errors)?;
        }
        writeln!(out)?;
        writeln!(
            out,
            "input: {:.1} MB; walk {:.2} s, read+extract+diff {secs:.2} s ({:.0} MB/s)",
            self.bytes as f64 / 1e6,
            walk.as_secs_f64(),
            if secs > 0.0 { self.bytes as f64 / 1e6 / secs } else { 0.0 }
        )?;
        let o = &self.oracle;
        match candidate {
            None => {
                writeln!(out, "ruff facts: defs {}, imports {}, calls {}", o.defs, o.imports, o.calls)?;
                writeln!(out, "candidate: none (our extractor is wired in by S1.5); nothing compared")?;
            }
            Some(name) => {
                let c = &self.candidate;
                writeln!(
                    out,
                    "facts (ruff/{name}): defs {}/{}, imports {}/{}, calls {}/{}",
                    o.defs, c.defs, o.imports, c.imports, o.calls, c.calls
                )?;
                writeln!(out, "mismatches: {}", self.mismatches())?;
                for cat in Category::ALL {
                    let n = self.counts[cat as usize];
                    if n > 0 {
                        writeln!(out, "  {:<22}{n:>10}", cat.as_str())?;
                    }
                }
                for cat in Category::ALL {
                    let ex = &self.examples[cat as usize];
                    if !ex.is_empty() {
                        writeln!(out, "\n{} (first {} of {}):", cat.as_str(), ex.len(), self.counts[cat as usize])?;
                        write_examples(out, ex)?;
                    }
                }
            }
        }
        if !self.oracle_skip_examples.is_empty() {
            writeln!(out, "\nskipped by ruff (first {} of {}):", self.oracle_skip_examples.len(), self.oracle_skipped())?;
            write_examples(out, &self.oracle_skip_examples)?;
        }
        if !self.read_error_examples.is_empty() {
            writeln!(out, "\nunreadable (first {} of {}):", self.read_error_examples.len(), self.read_errors)?;
            for e in &self.read_error_examples {
                writeln!(out, "  {}: {}", e.path, e.msg)?;
            }
        }
        Ok(())
    }
}

fn write_examples(out: &mut dyn Write, ex: &[Example]) -> io::Result<()> {
    for e in ex {
        writeln!(out, "  {}:{}: {}", e.path, e.line, e.msg)?;
    }
    Ok(())
}

/// [`Sink`] that counts into a [`Report`] and keeps examples for one file.
pub struct FileSink<'a> {
    report: &'a mut Report,
    path: &'a Path,
}

impl Sink for FileSink<'_> {
    fn mismatch(&mut self, cat: Category, line: u32, msg: fmt::Arguments<'_>) {
        let r = &mut *self.report;
        r.counts[cat as usize] += 1;
        Report::keep(&mut r.examples[cat as usize], r.max_examples, self.path, line, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(r: &Report, cand: Option<&str>) -> String {
        let mut out = Vec::new();
        r.write(&mut out, "corpus", cand, Duration::from_millis(250), Duration::from_millis(500)).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn keeps_first_n_examples_but_counts_all() {
        let mut r = Report::new(2);
        for line in 1..=5 {
            r.sink(Path::new("a.py")).mismatch(Category::CallMissing, line, format_args!("`f` in `<module>`"));
        }
        assert_eq!(r.counts[Category::CallMissing as usize], 5);
        let ex = &r.examples[Category::CallMissing as usize];
        assert_eq!(ex.len(), 2);
        assert_eq!((ex[0].line, ex[1].line), (1, 2));
        assert_eq!(r.mismatches(), 5);
    }

    #[test]
    fn merge_is_ordered_left_then_right() {
        let mut left = Report::new(3);
        left.files_seen = 2;
        left.bytes = 10;
        left.oracle.defs = 4;
        left.sink(Path::new("a.py")).mismatch(Category::DefLine, 1, format_args!("x"));
        left.sink(Path::new("a.py")).mismatch(Category::DefLine, 2, format_args!("y"));
        left.oracle_skip(Path::new("bad.py"), SkipKind::Syntax, 3, "nope");
        let mut right = Report::new(3);
        right.files_seen = 1;
        right.bytes = 5;
        right.oracle.defs = 1;
        right.read_errors = 1;
        for line in [7, 8] {
            right.sink(Path::new("b.py")).mismatch(Category::DefLine, line, format_args!("z"));
        }
        right.oracle_skip(Path::new("worse.py"), SkipKind::NotUtf8, 1, "bytes");
        let m = left.merge(right);
        assert_eq!(m.files_seen, 3);
        assert_eq!(m.bytes, 15);
        assert_eq!(m.oracle.defs, 5);
        assert_eq!(m.read_errors, 1);
        assert_eq!(m.counts[Category::DefLine as usize], 4);
        let ex: Vec<(&str, u32)> = m.examples[Category::DefLine as usize].iter().map(|e| (e.path.as_str(), e.line)).collect();
        assert_eq!(ex, [("a.py", 1), ("a.py", 2), ("b.py", 7)]);
        assert_eq!(m.oracle_skips, [1, 1]);
        assert_eq!(m.oracle_skip_examples.len(), 2);
    }

    #[test]
    fn renders_comparison_report() {
        let mut r = Report::new(5);
        r.files_seen = 3;
        r.bytes = 2_000_000;
        r.oracle = Totals { files: 2, defs: 10, imports: 4, calls: 30 };
        r.candidate = Totals { files: 2, defs: 9, imports: 4, calls: 31 };
        r.sink(Path::new("pkg/a.py")).mismatch(Category::DefMissing, 12, format_args!("function `A.f`"));
        r.sink(Path::new("pkg/b.py")).mismatch(Category::CallExtra, 3, format_args!("`g` in `<module>`"));
        r.oracle_skip(Path::new("py2.py"), SkipKind::Syntax, 1, "Simple statements must be separated by newlines or semicolons");
        let text = render(&r, Some("ours"));
        let want = "\
diff-ruff: corpus
files: 3 seen, 2 parsed by ruff, 1 skipped by ruff (syntax 1)
input: 2.0 MB; walk 0.25 s, read+extract+diff 0.50 s (4 MB/s)
facts (ruff/ours): defs 10/9, imports 4/4, calls 30/31
mismatches: 2
  defs.missing                   1
  calls.extra                    1

defs.missing (first 1 of 1):
  pkg/a.py:12: function `A.f`

calls.extra (first 1 of 1):
  pkg/b.py:3: `g` in `<module>`

skipped by ruff (first 1 of 1):
  py2.py:1: syntax: Simple statements must be separated by newlines or semicolons
";
        assert_eq!(text, want);
    }

    #[test]
    fn renders_oracle_only_report() {
        let mut r = Report::new(5);
        r.files_seen = 1;
        r.oracle = Totals { files: 1, defs: 2, imports: 3, calls: 4 };
        r.read_error(Path::new("gone.py"), &"No such file or directory");
        let text = render(&r, None);
        assert!(text.contains("files: 1 seen, 1 parsed by ruff, 1 unreadable\n"), "{text}");
        assert!(text.contains("ruff facts: defs 2, imports 3, calls 4\n"), "{text}");
        assert!(text.contains("candidate: none"), "{text}");
        assert!(text.contains("unreadable (first 1 of 1):\n  gone.py: No such file or directory\n"), "{text}");
        assert!(!text.contains("mismatches"), "{text}");
    }
}
