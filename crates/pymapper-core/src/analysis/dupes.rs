//! Duplication analysis (design: Goals; rule PM501 `duplicate-code`).
//!
//! Token-based clone detection in the MOSS style, in two stages:
//!
//! ```text
//!   extraction (per file, in the loader's pass, only when analysis.dupes is on)
//!     parser token events -> Normaliser (names/literals collapsed, imports dropped)
//!     -> one-byte codes (per-thread buffer) -> rolling k-gram hash -> winnowing
//!     -> Fingerprint {hash u32, file u32, pos u32}            the source is then dropped
//!
//!   find_clones (whole program)                                       phase
//!     key each fingerprint by its neighbours' hashes, sort by hash    dupes.seeds
//!     -> seeds: pairs of positions sharing a hash (all pairs in small buckets; in large
//!        ones, a path through the occurrences sharing the next / previous fingerprint:
//!        linear in n), sorted by (file A, file B, diagonal, position)
//!     -> re-read + re-lex every file with seeds once, in parallel     dupes.relex
//!        (kept: 1 byte of code + 1 byte of exact hash per token)
//!     -> per file A in parallel: verify each seed, extend it left and dupes.extend
//!        right over equal codes, split out runs of differing data, chain segments across
//!        small gaps, measure similarity, keep runs of >= min tokens
//!     -> re-read files with candidates once for line numbers;        dupes.locate
//!        keep runs spanning >= min lines in both copies
//!     -> clusters (copies in one file overlapping a common anchor)    dupes.group
//!        -> groups (union-find over pairs)
//!     -> one PM501 finding per copy, for its most similar pair
//! ```
//!
//! # Cost class: expensive (gated by `analysis.dupes`)
//!
//! - Off: zero cost. The loader monomorphises the extraction loop without the fingerprint
//!   visitor, so not even a branch per token is paid.
//! - Extraction: one byte per kept token in a per-thread buffer plus 8 bytes per token of k-gram
//!   hashes (reused across files), and 12 bytes per fingerprint retained. The density of
//!   winnowing is `2 / (w + 1)`: with the defaults (`k = 20`, `w = 31`) about one fingerprint
//!   per 16 tokens, i.e. under 1 byte per token retained.
//! - Cache: with `cache::load_with`, first-party segments store each file's fingerprints (8
//!   bytes per fingerprint, keyed by [`DupesParams::fingerprint_key`]); cached files get them
//!   without a read or parse. A segment without fingerprints of the key is extracted again
//!   once and rewritten.
//! - Matching: O(n log n) in fingerprints for the sorts (20 bytes per fingerprint while
//!   seeding), linear seeds (at most `(ALL_PAIRS_MAX - 1) / 2` per fingerprint in small
//!   buckets, two in large ones), 2 bytes per token of the files with seeds while extending,
//!   and at most two reads of each file (sources are never retained). Extension is linear in
//!   the matched tokens, also in periodic code (runs of close hits of one hash in one file
//!   seed once); no step compares all pairs of files or of clones.
//!
//! See "Duplication (S4.3d)" in `docs/benchmarks.md` for measured costs.
//!
//! # Parameters
//!
//! `min_tokens` (`thresholds.dupes-min-tokens`) is the winnowing guarantee threshold
//! `t = k + w - 1`: every shared run of at least `t` normalised tokens has a common
//! fingerprint, hence a seed. `k = clamp(t / 2, 1, 20)` is the noise threshold (shorter shared
//! runs are never seeds). A reported clone is at least `min_tokens` tokens and spans at least
//! `min_lines` lines (`thresholds.dupes-min-lines`) in both copies.
//!
//! # Normalisation and similarity
//!
//! Identifiers, numbers and strings each collapse to one code, so renamed copies match; the
//! reported similarity (the finding's confidence) is the share of aligned positions where the
//! tokens are identical, with tokens of bridged gaps counted as different. Token identity is
//! compared by an 8-bit hash of the token's bytes, so a differing token is taken as identical
//! with probability 1/256: the similarity may be overstated by about 0.4 points times the
//! share of differing tokens. Statement-level `import`/`from` statements are dropped (import
//! blocks would dominate the results).
//!
//! # Skipped code and trimming ([`analyse`])
//!
//! Stub files (`.pyi`) and, unless `analysis.include-tests-in-metrics`, test code (see
//! [`crate::analysis::testfiles`]) are left out by dropping their fingerprints before
//! matching (this also makes matching cheaper). When copies are located, each is trimmed: the
//! rest of a statement it starts in the middle of (first line only), leading / trailing tokens
//! on `@overload` signature lines ([`LineMask::overloads`]) and leading / trailing string
//! statements (docstrings). A pair whose trimmed copies fall below `min_tokens` or
//! `min_lines` is dropped, so runs of overload signatures are not reported. A finding's
//! symbol is the innermost class or function containing its whole copy ([`enclosing_def`]).
//!
//! Boilerplate is only reported when the two copies are identical: highly repetitive runs (a
//! period of at most 24 tokens covering 90% of the run, e.g. long `self.a = a` assignment
//! lists) and data (fewer than one token in 8 is a name or keyword, e.g. tables of tuples or
//! dict literals). Within a match, runs of at least 96 tokens of differing boilerplate are cut
//! out, so code next to a same-shaped table (a codec class before its mapping) is reported
//! without the table. Code that repeats with a period shorter than `min_tokens` (a long run
//! of like entries) is not reported as a clone of itself: each repetition is below the
//! minimum.

mod matcher;
mod norm;
#[cfg(test)]
mod tests;
mod winnow;

pub use matcher::{Best, DupesReport, DupesStats, find_clones, find_clones_masked};
pub use winnow::{Fingerprint, FingerprintScratch, FingerprintVisitor, Fingerprints, fingerprint_source};

use std::fs::File;
use std::io::Read;

use crate::analysis::testfiles;
use crate::config::Config;
use crate::extract::facts::def_flags;
use crate::index::Index;
use crate::rules::codes::DUPLICATE_CODE;
use crate::rules::{Arg, FileId, Findings, Location, Pass, RuleEngine, SymbolId};

/// Largest k-gram length.
const MAX_K: u32 = 20;

/// Bump whenever normalisation, hashing or winnowing changes the fingerprints taken for some
/// input: cached fingerprints with another version are not reused.
const FINGERPRINT_VERSION: u32 = 1;

/// What fingerprints depend on besides the source: `[version, k, w]`. Stored with cached
/// fingerprints; a load reuses them only when its key is equal.
pub type FingerprintKey = [u32; 3];

/// Detection parameters (from `thresholds.dupes-min-tokens` / `dupes-min-lines`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DupesParams {
    /// Smallest reported clone, in normalised tokens (at least 2).
    pub min_tokens: u32,
    /// Smallest reported clone, in lines (in both copies).
    pub min_lines: u32,
    /// Also match test code (`analysis.include-tests-in-metrics`; see
    /// [`crate::analysis::testfiles`]). Fingerprints do not depend on it.
    pub include_tests: bool,
}

impl Default for DupesParams {
    fn default() -> Self {
        DupesParams { min_tokens: 50, min_lines: 6, include_tests: false }
    }
}

impl DupesParams {
    /// Parameters from explicit thresholds; `min_tokens` below 2 (the smallest that makes a
    /// k-gram and a window) is raised to 2. The config accepts `dupes-min-tokens >= 10`, below
    /// which nearly every statement would be reported; smaller values are for tests.
    pub fn new(min_tokens: u32, min_lines: u32) -> DupesParams {
        DupesParams { min_tokens: min_tokens.max(2), min_lines, include_tests: false }
    }

    /// With `include_tests` set.
    pub fn with_tests(self, include_tests: bool) -> DupesParams {
        DupesParams { include_tests, ..self }
    }

    /// The parameters of `config` when `analysis.dupes` is on, else `None` (the analysis does
    /// not run and fingerprints should not be taken).
    pub fn from_config(config: &Config) -> Option<DupesParams> {
        let t = &config.thresholds;
        let tests = config.analysis.include_tests_in_metrics;
        config.analysis.dupes.then(|| DupesParams::new(t.dupes_min_tokens, t.dupes_min_lines).with_tests(tests))
    }

    /// The parameters for a run of `engine`: `None` when no rule of the duplication pass is
    /// enabled (PM501 ignored, or `analysis.dupes` off).
    pub fn from_engine(engine: &RuleEngine) -> Option<DupesParams> {
        let t = engine.thresholds();
        let tests = engine.include_tests_in_metrics();
        engine.should_run(Pass::Dupes).then(|| DupesParams::new(t.dupes_min_tokens, t.dupes_min_lines).with_tests(tests))
    }

    /// k-gram length (noise threshold).
    #[inline]
    pub fn k(&self) -> u32 {
        (self.min_tokens.max(2) / 2).clamp(1, MAX_K)
    }

    /// Winnowing window, in k-grams.
    #[inline]
    pub fn w(&self) -> u32 {
        self.min_tokens.max(2) - self.k() + 1
    }

    /// Key of the fingerprints taken with these parameters (`min_lines` does not matter).
    #[inline]
    pub fn fingerprint_key(&self) -> FingerprintKey {
        [FINGERPRINT_VERSION, self.k(), self.w()]
    }
}

/// One copy of a clone: a run of normalised tokens of a file and its lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Region {
    pub file: u32,
    /// First line (1-based).
    pub start_line: u32,
    /// Last line (inclusive).
    pub end_line: u32,
    /// 1-based byte column of the first token.
    pub col: u32,
    /// Token range `start_tok..end_tok` in the file's normalised token sequence.
    pub start_tok: u32,
    pub end_tok: u32,
}

impl Region {
    /// Lines spanned (inclusive).
    #[inline]
    pub fn lines(&self) -> u32 {
        self.end_line - self.start_line + 1
    }

    /// Normalised tokens.
    #[inline]
    pub fn tokens(&self) -> u32 {
        self.end_tok - self.start_tok
    }
}

/// Two copies of the same code. `a` comes first in (file, position) order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClonePair {
    pub a: Region,
    pub b: Region,
    /// Percentage of aligned positions with byte-identical tokens (100 = exact copy).
    pub similarity: u8,
}

/// Line ranges per file that reported copies must not start or end on (`@overload`
/// signatures): sorted `(file, first line, last line)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineMask {
    ranges: Vec<(u32, u32, u32)>,
}

impl LineMask {
    /// A mask of the given `(file, first line, last line)` ranges (any order).
    pub fn new(mut ranges: Vec<(u32, u32, u32)>) -> LineMask {
        ranges.sort_unstable();
        // Merge overlapping ranges of a file (nested overloads), so they are disjoint.
        let mut merged: Vec<(u32, u32, u32)> = Vec::with_capacity(ranges.len());
        for r in ranges {
            match merged.last_mut() {
                Some(m) if m.0 == r.0 && r.1 <= m.2.saturating_add(1) => m.2 = m.2.max(r.2),
                _ => merged.push(r),
            }
        }
        LineMask { ranges: merged }
    }

    /// The lines of every `@overload` definition (decorators included) of the files of
    /// `index`. O(defs).
    pub fn overloads(index: &Index) -> LineMask {
        let defs = &index.facts().defs;
        let mut ranges = Vec::new();
        for f in 0..index.file_count() {
            for g in index.file_facts(f).defs.range() {
                if defs.flags[g] & def_flags::OVERLOAD != 0 {
                    ranges.push((f, defs.start_line[g], defs.end_line[g]));
                }
            }
        }
        LineMask::new(ranges)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Whether `line` of `file` is masked. O(log ranges): ranges are disjoint.
    pub fn contains(&self, file: u32, line: u32) -> bool {
        let i = self.ranges.partition_point(|&(f, s, _)| (f, s) <= (file, line));
        i > 0 && {
            let (f, s, e) = self.ranges[i - 1];
            f == file && s <= line && line <= e
        }
    }
}

/// A source reader for [`find_clones`] that re-reads the files of `index` from disk.
pub fn index_reader(index: &Index) -> impl Fn(u32, &mut Vec<u8>) -> bool + Sync + '_ {
    move |f, buf| File::open(index.file_path(f)).and_then(|mut h| h.read_to_end(buf)).is_ok()
}

/// Runs the analysis over the fingerprints taken by
/// [`load_with`](crate::index::load_with) and appends its findings to `out`.
///
/// Stub files (`.pyi`: signatures only) and, unless `params.include_tests`, test code are left
/// out: their fingerprints are dropped before matching. Copies are trimmed off `@overload`
/// signatures and docstrings at their ends (see [`find_clones_masked`]).
pub fn analyse(index: &Index, mut fps: Fingerprints, params: &DupesParams, out: &mut Findings) -> DupesReport {
    let skip: Vec<bool> = (0..index.file_count())
        .map(|f| {
            index.file_path(f).as_os_str().as_encoded_bytes().ends_with(b".pyi")
                || (!params.include_tests && testfiles::is_test_file(index, f))
        })
        .collect();
    fps.rows.retain(|fp| !skip.get(fp.file as usize).copied().unwrap_or(false));
    drop(skip);
    let mask = LineMask::overloads(index);
    let report = find_clones_masked(fps, params, index_reader(index), &mask);
    findings_in(index, &report, out);
    report
}

/// Appends one PM501 finding per copy (member) of `report` to `out`, for its most similar
/// pair: at the first token of the member's own copy in that pair, `<lines> lines duplicated
/// in <file>:<line>` with that copy's lines and the other copy's start, and the pair's
/// similarity as confidence. `end_line` is the copy's last line; the other copy is in the
/// extra arguments `[File, U32 start line, U32 end line, Symbol]`. Without an index, symbols
/// are [`SymbolId::NONE`].
pub fn findings(report: &DupesReport, out: &mut Findings) {
    push_findings(None, report, out);
}

/// [`findings`] with symbols: a copy within one class or function gets the innermost one.
pub fn findings_in(index: &Index, report: &DupesReport, out: &mut Findings) {
    push_findings(Some(index), report, out);
}

fn push_findings(index: Option<&Index>, report: &DupesReport, out: &mut Findings) {
    let sym = |r: &Region| index.map_or(SymbolId::NONE, |ix| enclosing_def(ix, r));
    for best in &report.best {
        let (this, other) = (&best.this, &best.other);
        out.push_ext(
            DUPLICATE_CODE,
            Location::new(FileId(this.file), this.start_line, this.col),
            this.end_line,
            sym(this),
            best.similarity,
            &[Arg::U32(this.lines()), Arg::File(FileId(other.file)), Arg::U32(other.start_line)],
            &[Arg::File(FileId(other.file)), Arg::U32(other.start_line), Arg::U32(other.end_line), Arg::Symbol(sym(other))],
        );
    }
}

/// The innermost class or function of `r`'s file whose lines contain all of `r`'s, or
/// [`SymbolId::NONE`]. O(defs of the file).
pub fn enclosing_def(index: &Index, r: &Region) -> SymbolId {
    if r.file >= index.file_count() || index.file_node(r.file) == crate::index::NONE {
        return SymbolId::NONE;
    }
    let defs = &index.facts().defs;
    let range = index.file_facts(r.file).defs.range();
    let base = range.start;
    let mut best = SymbolId::NONE;
    // Pre-order: a containing def found later is nested in an earlier one.
    for g in base + 1..range.end {
        if defs.start_line[g] <= r.start_line && r.end_line <= defs.end_line[g] {
            best = SymbolId(index.file_node(r.file) + (g - base) as u32);
        }
    }
    best
}
