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

pub use matcher::{Best, DupesReport, DupesStats, find_clones};
pub use winnow::{Fingerprint, FingerprintScratch, FingerprintVisitor, Fingerprints, fingerprint_source};

use std::fs::File;
use std::io::Read;

use crate::config::Config;
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
}

impl Default for DupesParams {
    fn default() -> Self {
        DupesParams { min_tokens: 50, min_lines: 6 }
    }
}

impl DupesParams {
    /// Parameters from explicit thresholds; `min_tokens` below 2 (the smallest that makes a
    /// k-gram and a window) is raised to 2. The config accepts `dupes-min-tokens >= 10`, below
    /// which nearly every statement would be reported; smaller values are for tests.
    pub fn new(min_tokens: u32, min_lines: u32) -> DupesParams {
        DupesParams { min_tokens: min_tokens.max(2), min_lines }
    }

    /// The parameters of `config` when `analysis.dupes` is on, else `None` (the analysis does
    /// not run and fingerprints should not be taken).
    pub fn from_config(config: &Config) -> Option<DupesParams> {
        let t = &config.thresholds;
        config.analysis.dupes.then(|| DupesParams::new(t.dupes_min_tokens, t.dupes_min_lines))
    }

    /// The parameters for a run of `engine`: `None` when no rule of the duplication pass is
    /// enabled (PM501 ignored, or `analysis.dupes` off).
    pub fn from_engine(engine: &RuleEngine) -> Option<DupesParams> {
        let t = engine.thresholds();
        engine.should_run(Pass::Dupes).then(|| DupesParams::new(t.dupes_min_tokens, t.dupes_min_lines))
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

/// A source reader for [`find_clones`] that re-reads the files of `index` from disk.
pub fn index_reader(index: &Index) -> impl Fn(u32, &mut Vec<u8>) -> bool + Sync + '_ {
    move |f, buf| File::open(index.file_path(f)).and_then(|mut h| h.read_to_end(buf)).is_ok()
}

/// Runs the analysis over the fingerprints taken by
/// [`load_with`](crate::index::load_with) and appends its findings to `out`.
pub fn analyse(index: &Index, fps: Fingerprints, params: &DupesParams, out: &mut Findings) -> DupesReport {
    let report = find_clones(fps, params, index_reader(index));
    findings(&report, out);
    report
}

/// Appends one PM501 finding per copy (member) of `report` to `out`, for its most similar
/// pair: at the first token of the member's own copy in that pair, `<lines> lines duplicated
/// in <file>:<line>` with that copy's lines and the other copy's start, and the pair's
/// similarity as confidence.
pub fn findings(report: &DupesReport, out: &mut Findings) {
    for best in &report.best {
        let (this, other) = (&best.this, &best.other);
        out.push(
            DUPLICATE_CODE,
            Location::new(FileId(this.file), this.start_line, this.col),
            SymbolId::NONE,
            best.similarity,
            &[Arg::U32(this.lines()), Arg::File(FileId(other.file)), Arg::U32(other.start_line)],
        );
    }
}
