//! Finding output (design: Checks / CLI, "Output"): concise, JSON, SARIF 2.1.0 and GitHub
//! annotations, the summary footer, exit codes and baselines.
//!
//! # Pipeline
//!
//! ```text
//!   Findings (filtered by RuleEngine::filter, any order)
//!     --> Report::new         one path per file with findings, rendered once into a pool;
//!                             finding order = indices sorted by (path, line, col, rule,
//!                             symbol, confidence), ties broken by the rendered message
//!     --> Report::apply_baseline   drop findings recorded in a baseline ("new only" mode)
//!     --> write(format)       into our own 64 KiB buffer, flushed in large writes; messages
//!                             are rendered into one reused scratch buffer (no per-line
//!                             allocation); a closed pipe (EPIPE) stops output quietly
//!     --> Report::exit_status 0 clean, 1 findings at/above the confidence gate
//! ```
//!
//! # Paths and columns
//!
//! Paths are shown relative to the project root ([`IndexContext`]); files outside it keep
//! their absolute path. Columns are 1-based and counted in bytes from the start of the line
//! (sources are not retained, so they are not converted to characters). They differ from
//! character columns only when non-ASCII text precedes the finding on its line, which does not
//! happen for definitions and imports (indentation is ASCII).
//!
//! A finding without a line prints `path: CODE message`; without a file, `CODE message`.
//! Invalid UTF-8 in paths and qualified ids is replaced with U+FFFD everywhere (all formats and
//! baseline keys), so the formats agree and baselines round-trip.
//!
//! # Use (`pymapper check`)
//!
//! ```ignore
//! let stats = engine.filter(&mut findings, &suppressions, false, |file, i| ...);
//! let ctx = IndexContext::new(&index, &root, &config.contracts);
//! let mut report = Report::new(&findings, &ctx);
//! if let Some(b) = &baseline { report.apply_baseline(b); }
//! let opts = Options { format: config.output_format, min_confidence: config.min_confidence, footer: true };
//! report.write(&opts, &Summary { files_checked, filter: stats }, &mut std::io::stdout().lock())?;
//! std::process::ExitCode::from(report.exit_status(opts.min_confidence).code())
//! ```
//!
//! # Formats
//!
//! - `concise`: `path:line:col: CODE message`, confidence appended when below 100, then a
//!   summary footer.
//! - `json`: one document, schema version [`JSON_VERSION`] (see [`json`]).
//! - `sarif`: SARIF 2.1.0, one run, rule metadata for the rules that have results.
//! - `github`: GitHub Actions workflow commands (`::warning file=...::message`), then the footer.
//!
//! # Baselines
//!
//! See [`Baseline`]: findings are keyed by rule + stable id (the symbol's qualified id, else the
//! file path) + the stable message arguments (names, symbols, files, contracts; never numbers),
//! with a count per key.

mod baseline;
mod buf;
mod concise;
mod context;
mod github;
pub mod json;
mod jsonread;
mod sarif;

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::io;

pub use baseline::{Baseline, BaselineError};
pub use buf::{Out, Outcome, is_broken_pipe};
pub use context::{Context, IndexContext};

pub use crate::config::OutputFormat;
use crate::rules::codes::UNUSED_IMPORT;
use crate::rules::{Arg, FileId, FilterStats, Finding, Findings, SymbolId};

/// Version of the JSON output schema (bumped on incompatible changes; see [`json`] for the
/// history).
pub const JSON_VERSION: u32 = 2;

/// Initial capacity of the reused per-line scratch buffers (messages, ids, keys), so typical
/// lines never grow them.
pub(crate) const SCRATCH: usize = 512;

/// Process exit status of `check` (design: "Exit 0 clean / 1 findings / 2 usage or fatal").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExitStatus {
    /// No finding at or above the confidence gate.
    Clean = 0,
    /// At least one finding at or above the confidence gate.
    Findings = 1,
    /// Usage error or fatal error.
    Error = 2,
}

impl ExitStatus {
    /// The numeric exit code.
    #[inline]
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Counts for the summary footer and the JSON summary, beyond the findings themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Files analysed (0 = unknown; then the footer does not mention it).
    pub files_checked: u32,
    /// What [`crate::rules::RuleEngine::filter`] dropped or kept.
    pub filter: FilterStats,
}

/// Rendering options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Output format.
    pub format: OutputFormat,
    /// Confidence gate (0-100): findings at or above it make the exit status 1. Findings below
    /// it are still written if present (JSON consumers filter on `confidence`).
    pub min_confidence: u8,
    /// Write the summary footer (concise and github formats).
    pub footer: bool,
}

impl Options {
    /// `format` with the default gate and a footer.
    pub fn new(format: OutputFormat) -> Options {
        Options { format, min_confidence: crate::config::DEFAULT_MIN_CONFIDENCE, footer: true }
    }
}

/// Display paths of the files that have findings, rendered once, ranked by path bytes.
#[derive(Debug, Default)]
struct Paths {
    pool: Vec<u8>,
    /// `pool` range of each rank's path.
    spans: Vec<(u32, u32)>,
    /// File id -> rank (`u32::MAX` = no findings). Dense: 4 bytes per file up to the largest id.
    rank_of: Vec<u32>,
}

const NO_RANK: u32 = u32::MAX;

impl Paths {
    fn build(findings: &Findings, ctx: &dyn Context) -> Paths {
        let mut p = Paths::default();
        let items = findings.as_slice();
        // Size every table once: mark the files with findings, then render each distinct path
        // once, in file id order (slot = index into `files`).
        let Some(max) = items.iter().filter(|f| f.loc.file != FileId::NONE).map(|f| f.loc.file.0).max() else {
            return p;
        };
        p.rank_of = vec![NO_RANK; max as usize + 1];
        let mut distinct = 0usize;
        for f in items {
            if f.loc.file != FileId::NONE && p.rank_of[f.loc.file.0 as usize] == NO_RANK {
                p.rank_of[f.loc.file.0 as usize] = 0;
                distinct += 1;
            }
        }
        let mut files: Vec<FileId> = Vec::with_capacity(distinct);
        p.spans.reserve_exact(distinct);
        p.pool.reserve(distinct * 32);
        for i in 0..=max {
            if p.rank_of[i as usize] == NO_RANK {
                continue;
            }
            let start = p.pool.len() as u32;
            ctx.write_path(FileId(i), &mut p.pool);
            lossy_tail(&mut p.pool, start as usize);
            p.spans.push((start, p.pool.len() as u32 - start));
            files.push(FileId(i));
        }
        // Rank slots by path bytes (file id breaks ties between equal display paths).
        let mut order: Vec<u32> = (0..files.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| p.slot(a).cmp(p.slot(b)).then(files[a as usize].cmp(&files[b as usize])));
        let spans: Vec<(u32, u32)> = order.iter().map(|&s| p.spans[s as usize]).collect();
        for (rank, &slot) in order.iter().enumerate() {
            p.rank_of[files[slot as usize].0 as usize] = rank as u32;
        }
        p.spans = spans;
        p
    }

    #[inline]
    fn slot(&self, s: u32) -> &[u8] {
        let (start, len) = self.spans[s as usize];
        &self.pool[start as usize..(start + len) as usize]
    }

    /// Rank of a file (files without a path sort first, as `NONE` ranks below every path).
    #[inline]
    fn rank(&self, file: FileId) -> u32 {
        if file == FileId::NONE { 0 } else { self.rank_of[file.0 as usize] + 1 }
    }

    /// Display path of a file that has findings (empty for [`FileId::NONE`]).
    #[inline]
    fn path(&self, file: FileId) -> &[u8] {
        if file == FileId::NONE { &[] } else { self.slot(self.rank_of[file.0 as usize]) }
    }
}

/// Findings prepared for output: sorted deterministically, optionally filtered by a baseline.
/// Holds 4 bytes per finding plus each distinct path once; the findings are borrowed.
pub struct Report<'a> {
    findings: &'a Findings,
    ctx: &'a dyn Context,
    paths: Paths,
    /// Indices into `findings.as_slice()`, in output order.
    order: Vec<u32>,
    /// Findings removed by [`Report::apply_baseline`].
    baselined: u32,
}

impl<'a> Report<'a> {
    /// Sorts `findings` for output: by display path, line, column, rule, symbol id and
    /// confidence; findings equal in all of those are ordered by their rendered message, so the
    /// order does not depend on how analyses scheduled their work.
    pub fn new(findings: &'a Findings, ctx: &'a dyn Context) -> Report<'a> {
        let paths = Paths::build(findings, ctx);
        let items = findings.as_slice();
        let mut order: Vec<u32> = (0..items.len() as u32).collect();
        let key = |f: &Finding| (paths.rank(f.loc.file), f.loc.line, f.loc.col, f.rule, f.symbol, f.confidence);
        order.sort_unstable_by(|&a, &b| key(&items[a as usize]).cmp(&key(&items[b as usize])));
        // Break remaining ties by message. Ties are rare (same rule, place and symbol), so
        // messages are only rendered for them.
        let mut ma = String::new();
        let mut mb = String::new();
        let mut i = 0;
        while i < order.len() {
            let k = key(&items[order[i] as usize]);
            let mut j = i + 1;
            while j < order.len() && key(&items[order[j] as usize]) == k {
                j += 1;
            }
            if j - i > 1 {
                order[i..j].sort_by(|&a, &b| {
                    compare_messages(findings, ctx, &items[a as usize], &items[b as usize], &mut ma, &mut mb)
                });
            }
            i = j;
        }
        Report { findings, ctx, paths, order, baselined: 0 }
    }

    /// Number of findings to be written.
    #[inline]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether there is nothing to write.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Findings removed by baselines so far.
    #[inline]
    pub fn baselined(&self) -> u32 {
        self.baselined
    }

    /// The findings in output order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a Finding> + '_ {
        let items = self.findings.as_slice();
        self.order.iter().map(move |&i| &items[i as usize])
    }

    /// The display path of a finding's file, as shown in the output (empty without a file).
    pub fn path(&self, f: &Finding) -> &[u8] {
        self.paths.path(f.loc.file)
    }

    /// Number of distinct files with findings still in the report.
    pub fn file_count(&self) -> usize {
        let mut n = 0;
        let mut last = None;
        for f in self.iter() {
            if f.loc.file != FileId::NONE && last != Some(f.loc.file) {
                n += 1;
                last = Some(f.loc.file);
            }
        }
        n
    }

    /// Number of findings at or above the gate.
    pub fn gated_count(&self, min_confidence: u8) -> usize {
        self.iter().filter(|f| f.confidence >= min_confidence).count()
    }

    /// Exit status: [`ExitStatus::Findings`] if any finding is at or above the gate.
    pub fn exit_status(&self, min_confidence: u8) -> ExitStatus {
        if self.iter().any(|f| f.confidence >= min_confidence) { ExitStatus::Findings } else { ExitStatus::Clean }
    }

    /// Drops the findings recorded in `baseline` ("new findings only"). Where more findings
    /// share a key than the baseline counted, the first ones in output order are dropped and
    /// the rest reported. Returns the number dropped.
    pub fn apply_baseline(&mut self, baseline: &Baseline) -> u32 {
        let dropped = baseline.filter(self);
        self.baselined += dropped;
        dropped
    }

    /// Writes the report in `opts.format` to `w`, buffered. Returns [`Outcome::Closed`] if the
    /// reader went away (EPIPE); other I/O errors are returned.
    pub fn write(&self, opts: &Options, summary: &Summary, w: &mut dyn io::Write) -> io::Result<Outcome> {
        let mut out = Out::new(w);
        match opts.format {
            OutputFormat::Concise => concise::write(self, opts, summary, &mut out),
            OutputFormat::Json => json::write_report(self, opts, summary, &mut out),
            OutputFormat::Sarif => sarif::write(self, &mut out),
            OutputFormat::Github => github::write(self, opts, summary, &mut out),
        }
        out.finish()
    }

    /// Writes the baseline of the findings in this report (see [`Baseline`]).
    pub fn write_baseline(&self, w: &mut dyn io::Write) -> io::Result<Outcome> {
        let mut out = Out::new(w);
        baseline::write(self, &mut out);
        out.finish()
    }

    /// Message of a finding rendered into `buf` (cleared first).
    pub(crate) fn message(&self, f: &Finding, buf: &mut String) {
        buf.clear();
        // Writing into a String cannot fail; resolvers only fail if the sink does.
        let _ = self.findings.write_message(f, self.ctx, buf);
    }

    /// Stable id of a finding's symbol rendered into `buf` (cleared first; empty without one).
    /// An unused import (PM405) is shown as `pkg.mod:<bound name>`: its symbol is the importing
    /// module and the bound name its first argument. (Baseline keys are unchanged: module id
    /// plus the name as detail.)
    pub(crate) fn symbol_id(&self, f: &Finding, buf: &mut Vec<u8>) {
        buf.clear();
        if f.symbol != SymbolId::NONE {
            self.ctx.write_symbol_id(f.symbol, buf);
            if f.rule == UNUSED_IMPORT
                && let Some(&Arg::Name(n)) = self.findings.args(f).first()
            {
                buf.push(b':');
                let mark = buf.len();
                if self.ctx.write_name(n, &mut VecWriter(buf)).is_err() {
                    buf.truncate(mark - 1);
                }
            }
            lossy_tail(buf, 0);
        }
    }

    /// Last line of a finding's span: the one the analysis recorded, else the end of its
    /// symbol's definition when the finding is about a class or function, else its line (0 =
    /// unknown).
    pub(crate) fn end_line(&self, f: &Finding) -> u32 {
        if f.end_line != 0 {
            return f.end_line.max(f.loc.line);
        }
        if f.symbol != SymbolId::NONE && f.loc.line != 0 {
            let end = self.ctx.def_end_line(f.symbol);
            if end >= f.loc.line {
                return end;
            }
        }
        f.loc.line
    }

    pub(crate) fn findings(&self) -> &'a Findings {
        self.findings
    }

    pub(crate) fn ctx(&self) -> &'a dyn Context {
        self.ctx
    }

    /// Keeps the findings for which `keep` is true (e.g. to hide findings below the gate).
    pub fn retain(&mut self, mut keep: impl FnMut(&Finding) -> bool) {
        let items = self.findings.as_slice();
        self.order.retain(|&i| keep(&items[i as usize]));
    }
}

/// `fmt::Write` into a byte buffer (no intermediate `String`).
struct VecWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for VecWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Replaces invalid UTF-8 in `buf[start..]` with U+FFFD, so every format (and the baseline
/// file) shows the same text. Free for valid UTF-8, which is the norm.
pub(crate) fn lossy_tail(buf: &mut Vec<u8>, start: usize) {
    if std::str::from_utf8(&buf[start..]).is_ok() {
        return;
    }
    let fixed = String::from_utf8_lossy(&buf[start..]).into_owned();
    buf.truncate(start);
    buf.extend_from_slice(fixed.as_bytes());
}

fn compare_messages(
    findings: &Findings,
    ctx: &dyn Context,
    a: &Finding,
    b: &Finding,
    ma: &mut String,
    mb: &mut String,
) -> Ordering {
    ma.clear();
    mb.clear();
    let _ = findings.write_message(a, ctx, ma);
    let _ = findings.write_message(b, ctx, mb);
    ma.as_bytes().cmp(mb.as_bytes())
}
