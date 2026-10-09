//! Config-file evidence (dead code): identifiers and dotted / `module:attr` / slash paths in
//! non-Python config files (YAML, TOML, JSON, INI, ...), as evidence hashes.
//!
//! Config files name code by string: `module_name: pkg.tools`, `source_name: search`,
//! `factory = "pkg.app:create_app"`, `path: src/pkg/job.py`. No format is parsed: one pass over
//! the bytes takes every maximal run of identifier bytes, `.`, `:` and `/` and records
//!
//! - the evidence hash ([`evidence_hash`]) of each identifier segment (`search`), and
//! - for a run of two segments or more whose separators are `.` and at most one `:`, the hash
//!   of each prefix of two segments or more with `:` read as `.` (`pkg.app`, `pkg.app.create_app`):
//!   the same path hashes the extractor records for string literals;
//! - for a `/` path (`src/pkg/job.py`), the hash of each suffix of two segments or more with
//!   `/` read as `.` and a trailing `.py` dropped (`pkg.job`, `src.pkg.job`): file paths name
//!   modules relative to some directory.
//!
//! Runs with an empty segment (`https://x`, `a..b`) give their segments only. Per file the
//! hashes are sorted and unique, 4 bytes each; sources are dropped after tokenizing.

use rayon::prelude::*;

use super::facts::evidence_hash;
use crate::walk::PathList;

/// Config files larger than this are skipped (data dumps, not configuration).
pub const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// Longest run considered as a path; longer runs give their segments only.
const MAX_RUN: usize = 512;
/// Most segments of a path whose prefixes / suffixes get a path hash.
const MAX_SEGS: usize = 8;

/// Counters of one collection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfigStats {
    /// Files tokenized (or taken from the cache).
    pub files: u32,
    /// Files skipped: larger than [`MAX_CONFIG_BYTES`].
    pub too_large: u32,
    /// Files that could not be read.
    pub unreadable: u32,
    /// Files skipped as tool output ([`is_tool_output`]).
    pub generated: u32,
    /// Files taken from the cache.
    pub cached: u32,
    /// Source bytes tokenized (cache misses).
    pub bytes: u64,
}

/// Evidence hashes of a set of config files: per file a sorted, unique run of hashes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigEvidence {
    hashes: Vec<u32>,
    /// Start of each file's run, plus the end.
    off: Vec<u32>,
    pub stats: ConfigStats,
}

impl ConfigEvidence {
    /// No files.
    pub fn new() -> ConfigEvidence {
        ConfigEvidence { hashes: Vec::new(), off: vec![0], stats: ConfigStats::default() }
    }

    /// Appends one file's hashes (sorted and deduplicated here).
    pub fn push_file(&mut self, hashes: &[u32]) {
        if self.off.is_empty() {
            self.off.push(0);
        }
        let at = self.hashes.len();
        self.hashes.extend_from_slice(hashes);
        let tail = &mut self.hashes[at..];
        tail.sort_unstable();
        let mut w = at;
        for r in at..self.hashes.len() {
            if w == at || self.hashes[r] != self.hashes[w - 1] {
                self.hashes[w] = self.hashes[r];
                w += 1;
            }
        }
        self.hashes.truncate(w);
        self.off.push(u32::try_from(self.hashes.len()).unwrap_or(u32::MAX));
    }

    /// Number of files.
    pub fn files(&self) -> usize {
        self.off.len().saturating_sub(1)
    }

    /// Sorted unique hashes of file `i`.
    #[inline]
    pub fn file(&self, i: usize) -> &[u32] {
        &self.hashes[self.off[i] as usize..self.off[i + 1] as usize]
    }

    /// Whether any file contains hash `h`.
    pub fn contains(&self, h: u32) -> bool {
        (0..self.files()).any(|i| self.file(i).binary_search(&h).is_ok())
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.hashes.capacity() + self.off.capacity()) * 4
    }

    /// Reads and tokenizes every file of `paths` in parallel (no cache), skipping files over
    /// [`MAX_CONFIG_BYTES`] and unreadable ones.
    pub fn collect(paths: &PathList) -> ConfigEvidence {
        let per_file: Vec<FileTokens> = (0..paths.len()).into_par_iter().map(|i| read_tokens(paths.get(i))).collect();
        let mut ev = ConfigEvidence::new();
        for t in per_file {
            ev.add(t);
        }
        ev
    }

    /// Adds one file's result.
    pub(crate) fn add(&mut self, t: FileTokens) {
        match t {
            FileTokens::Hashes(h, bytes) => {
                self.push_file(&h);
                self.stats.files += 1;
                self.stats.bytes += bytes;
            }
            FileTokens::Cached(h) => {
                self.push_file(&h);
                self.stats.files += 1;
                self.stats.cached += 1;
            }
            FileTokens::TooLarge => self.stats.too_large += 1,
            FileTokens::Unreadable => self.stats.unreadable += 1,
            FileTokens::Generated => self.stats.generated += 1,
        }
    }
}

/// What reading one config file gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileTokens {
    /// Hashes (unsorted) and the bytes read.
    Hashes(Vec<u32>, u64),
    /// Hashes from the cache.
    Cached(Vec<u32>),
    TooLarge,
    Unreadable,
    /// Tool output ([`is_tool_output`]).
    Generated,
}

/// Whether `src` (a config file's contents) is the output of a tool that lists code by name
/// rather than configuration: pymapper's own baselines and JSON / SARIF reports, and
/// coverage.py JSON reports. Only the first 512 bytes of JSON files are looked at.
///
/// A coverage.py report (`coverage json`, format 3) starts with a `"meta"` object such as
/// `{"meta": {"format": 3, "version": "7.15.2", "timestamp": ..., "branch_coverage": false,
/// "show_contexts": false}, "files": ...}` and, since 7.5, names every function and class of
/// every file: read as configuration it would mark all of them used. The report configured as
/// `analysis.coverage` is also left out of the config files by path, whatever its contents.
pub fn is_tool_output(path: &std::path::Path, src: &[u8]) -> bool {
    let json = path.extension().is_some_and(|e| e == "json" || e == "sarif");
    if !json {
        return false;
    }
    let head = &src[..src.len().min(512)];
    let has = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
    let coverage_py =
        has(b"\"meta\"") && has(b"\"format\"") && (has(b"\"show_contexts\"") || has(b"\"branch_coverage\""));
    has(b"pymapper") || coverage_py || has(b"\"coverage_version\"")
}

/// Tokenizes one file's contents (sorted, unique hashes), or `Generated` for tool output.
pub(crate) fn tokens_of(path: &std::path::Path, src: &[u8]) -> FileTokens {
    if src.len() as u64 > MAX_CONFIG_BYTES {
        return FileTokens::TooLarge;
    }
    if is_tool_output(path, src) {
        return FileTokens::Generated;
    }
    let mut out = Vec::new();
    config_tokens(src, &mut out);
    out.sort_unstable();
    out.dedup();
    FileTokens::Hashes(out, src.len() as u64)
}

/// Reads and tokenizes `path` (size-checked first).
pub(crate) fn read_tokens(path: &std::path::Path) -> FileTokens {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > MAX_CONFIG_BYTES => return FileTokens::TooLarge,
        Ok(_) => {}
        Err(_) => return FileTokens::Unreadable,
    }
    match std::fs::read(path) {
        Ok(src) => tokens_of(path, &src),
        Err(_) => FileTokens::Unreadable,
    }
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

#[inline]
fn is_sep(b: u8) -> bool {
    b == b'.' || b == b':' || b == b'/'
}

/// Appends the evidence hashes of `src` (see the module docs) to `out`, unsorted and possibly
/// with duplicates. No allocation besides `out`.
pub fn config_tokens(src: &[u8], out: &mut Vec<u32>) {
    let mut buf = [0u8; MAX_RUN];
    let mut i = 0;
    while i < src.len() {
        if !is_ident_byte(src[i]) || (i > 0 && is_ident_byte(src[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < src.len() && (is_ident_byte(src[i]) || is_sep(src[i])) {
            i += 1;
        }
        let mut end = i;
        while end > start && is_sep(src[end - 1]) {
            end -= 1;
        }
        run_hashes(&src[start..end], &mut buf, out);
    }
}

/// Hashes of one run (identifier bytes and separators, no trailing separator).
fn run_hashes(run: &[u8], buf: &mut [u8; MAX_RUN], out: &mut Vec<u32>) {
    let mut segs = 0usize;
    let mut valid = true;
    for seg in run.split(|&b| is_sep(b)) {
        if seg.is_empty() || seg[0].is_ascii_digit() {
            valid &= !seg.is_empty();
            continue;
        }
        out.push(evidence_hash(seg));
        segs += 1;
    }
    if !valid || segs < 2 || run.len() > MAX_RUN {
        return;
    }
    // Every segment must be an identifier for a path.
    if run.split(|&b| is_sep(b)).any(|seg| seg[0].is_ascii_digit()) {
        return;
    }
    let slashes = run.contains(&b'/');
    let colons = run.iter().filter(|&&b| b == b':').count();
    if slashes {
        if colons > 0 || run.contains(&b'.') && !run.ends_with(b".py") {
            return;
        }
        let body = run.strip_suffix(b".py").unwrap_or(run);
        let n = body.len();
        buf[..n].copy_from_slice(body);
        for b in &mut buf[..n] {
            if *b == b'/' {
                *b = b'.';
            }
        }
        // Suffixes of two segments or more: start after each separator, longest last.
        let mut cuts = 0;
        for k in (0..n).rev() {
            if buf[k] == b'.' {
                cuts += 1;
                if (2..MAX_SEGS).contains(&cuts) {
                    out.push(evidence_hash(&buf[k + 1..n]));
                }
            }
        }
        if (1..MAX_SEGS).contains(&cuts) {
            out.push(evidence_hash(&buf[..n]));
        }
        return;
    }
    if colons > 1 {
        return;
    }
    let n = run.len();
    buf[..n].copy_from_slice(run);
    let mut seen = 0;
    for k in 0..n {
        if buf[k] == b':' {
            buf[k] = b'.';
        }
        if buf[k] == b'.' {
            seen += 1;
            if (2..=MAX_SEGS).contains(&seen) {
                out.push(evidence_hash(&buf[..k]));
            }
        }
    }
    if seen < MAX_SEGS {
        out.push(evidence_hash(&buf[..n]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(src: &str) -> Vec<u32> {
        let mut out = Vec::new();
        config_tokens(src.as_bytes(), &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn has(h: &[u32], s: &str) -> bool {
        h.binary_search(&evidence_hash(s.as_bytes())).is_ok()
    }

    #[test]
    fn yaml_toml_json_and_paths() {
        let yaml = "servers:\n  - module_name: aiml_ais_mcp.elnb.tools\n    source_name: get_doc_by_id\n  - pipe: cogito_evaluations.pipeline.core_memory:CoreMemoryPipeline\n";
        let h = hashes(yaml);
        for yes in [
            "servers",
            "module_name",
            "get_doc_by_id",
            "aiml_ais_mcp",
            "tools",
            "aiml_ais_mcp.elnb",
            "aiml_ais_mcp.elnb.tools",
            "cogito_evaluations.pipeline.core_memory",
            "cogito_evaluations.pipeline.core_memory.CoreMemoryPipeline",
            "CoreMemoryPipeline",
        ] {
            assert!(has(&h, yes), "{yes}");
        }
        assert!(!has(&h, "elnb.tools") && !has(&h, "core_memory:CoreMemoryPipeline"));
        let h = hashes("[tool.x]\nfactory = \"pkg.app:create_app\"\nn = 3.14\n");
        assert!(has(&h, "pkg.app") && has(&h, "pkg.app.create_app") && has(&h, "factory") && has(&h, "tool.x"));
        assert!(!has(&h, "3.14") && !has(&h, "14"));
        let h = hashes("{\"handler\": \"my_func\", \"url\": \"https://example.com/a/b\", \"file\": \"src/pkg/job.py\"}");
        assert!(has(&h, "my_func") && has(&h, "handler") && has(&h, "example"));
        // A URL gives its segments only; a slash path gives module suffixes.
        assert!(!has(&h, "https.example.com") && !has(&h, "example.com"));
        assert!(has(&h, "pkg.job") && has(&h, "src.pkg.job") && !has(&h, "job.py"));
        // Two colons, empty segments and digit-led segments are not paths.
        let h = hashes("a:b:c x..y v1.2.3 pkg.0bad");
        assert!(!has(&h, "a.b") && !has(&h, "x.y") && !has(&h, "v1.2") && !has(&h, "pkg.0bad"));
        assert!(has(&h, "a") && has(&h, "x") && has(&h, "pkg"));
    }

    #[test]
    fn tool_output_is_not_configuration() {
        use std::path::Path;
        let baseline = b"{\n  \"version\": 1,\n  \"tool\": \"pymapper\",\n  \"entries\": [{\"code\": \"PM404\", \"id\": \"app.util\"}]}";
        assert_eq!(tokens_of(Path::new("baseline.json"), baseline), FileTokens::Generated);
        let cov = b"{\"meta\": {\"format\": 3, \"version\": \"7.6\", \"coverage_version\": \"7.6\"}, \"files\": {}}";
        assert_eq!(tokens_of(Path::new("coverage.json"), cov), FileTokens::Generated);
        // Real coverage.py output (7.15.2) has no `coverage_version` key.
        let cov = b"{\"meta\": {\"format\": 3, \"version\": \"7.15.2\", \"timestamp\": \"2026-10-09T10:00:00\", \
                    \"branch_coverage\": false, \"show_contexts\": false}, \"files\": {\"app/util.py\": \
                    {\"functions\": {\"helper\": {}}}}}";
        assert_eq!(tokens_of(Path::new("coverage.json"), cov), FileTokens::Generated);
        let cov = b"{\"meta\":{\"format\":3,\"version\":\"7.4\",\"branch_coverage\":true},\"files\":{}}";
        assert_eq!(tokens_of(Path::new("cov.json"), cov), FileTokens::Generated);
        // A `meta` object alone is not enough.
        assert!(matches!(tokens_of(Path::new("app.json"), b"{\"meta\": {\"format\": 1}}"), FileTokens::Hashes(..)));
        // Only JSON is sniffed: `[tool.pymapper]` in a TOML file is configuration.
        assert!(matches!(tokens_of(Path::new("pyproject.toml"), b"[tool.pymapper]\nx = 1\n"), FileTokens::Hashes(..)));
        assert!(matches!(tokens_of(Path::new("app.json"), b"{\"handler\": \"f\"}"), FileTokens::Hashes(..)));
    }

    #[test]
    fn collect_skips_large_and_unreadable_files() {
        let dir = std::env::temp_dir().join(format!("pymapper-cfg-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("a.yaml"), "name: alpha_func\n").expect("write");
        let mut big = String::from("name: beta_func\n");
        while big.len() as u64 <= MAX_CONFIG_BYTES {
            big.push_str("# padding padding padding padding padding padding padding padding\n");
        }
        std::fs::write(dir.join("big.yaml"), &big).expect("write");
        let mut paths = PathList::default();
        for name in ["a.yaml", "big.yaml", "missing.yaml"] {
            paths.push(dir.join(name).as_os_str().as_encoded_bytes()).expect("push");
        }
        let ev = ConfigEvidence::collect(&paths);
        assert_eq!(ev.files(), 1);
        assert_eq!((ev.stats.files, ev.stats.too_large, ev.stats.unreadable), (1, 1, 1));
        assert!(ev.contains(evidence_hash(b"alpha_func")));
        assert!(!ev.contains(evidence_hash(b"beta_func")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
