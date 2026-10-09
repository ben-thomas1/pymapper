//! Coverage evidence (design: Dead code (h)): a coverage.py JSON report (`coverage json`)
//! mapped to the definitions and modules of the index.
//!
//! ```text
//!   report bytes ──> json::Reader (one pass, no tree)
//!     "files": { path: { "executed_lines": [..], "missing_lines": [..], ... }, ... }
//!        │  path ──> FileId: absolute, or joined with each base directory (report's
//!        │           directory, project root), lexically normalised; else the unique file
//!        │           with the longest matching path suffix (reports made in another checkout)
//!        └─ lines ──> per def / module of the file: Executed, NotExecuted or unknown
//! ```
//!
//! # What counts as executed
//!
//! coverage.py reports *statements* by their first line (a multi-line statement, such as a
//! decorated `def` with a long signature, is reported at its first line), split into
//! `executed_lines`, `missing_lines` and excluded lines (`# pragma: no cover`, listed in
//! neither). For a definition with header lines `start_line..=name_line` (decorators to name)
//! and body lines `name_line + 1..=end_line`:
//!
//! - a function is **executed** when one of its body lines was executed (its body ran at least
//!   once; a `def` line itself runs when the enclosing scope does, so it proves nothing);
//! - a function is **not executed** when no body line was executed and either a body line is
//!   missing or a header line is (the `def` statement itself never ran: module never imported,
//!   branch not taken). One-line functions (`def f(): ...`) and bodies that are entirely
//!   excluded are unknown unless their header is missing;
//! - a class is never marked executed by its own lines (a class body runs at import); a live
//!   method makes it live through the usual propagation. It is **not executed** when its header
//!   is missing, or when it has direct methods / nested classes, none is executed or unknown and
//!   at least one is not executed (none of its code ever ran);
//! - a module is **executed** when any of its lines was executed (it was imported or run), and
//!   **not executed** when the report lists it with missing lines and no executed line (an
//!   unimported file of coverage's `source`).
//!
//! Files absent from the report, or listed under a path that matches no analysed file, give no
//! evidence. When two report entries map to the same file, executed wins.
//!
//! A line number beyond the end of its file marks the file as *stale* ([`CoverageStats`]): the
//! report was made from other sources and the mapping may be wrong (the CLI warns).
//!
//! # Cost: O(report bytes + defs of matched files * log lines)
//!
//! One pass over the report with reused buffers (per file: its line arrays, sorted when they
//! are not already, plus two bytes per def of the file); a hash map from path bytes to file for
//! the analysed files; a sorted (basename hash, file) array for suffix matching, built only when
//! a path is not found directly. The result is two bit sets over the nodes (2 bits per node).

mod json;

use std::path::Path;

use super::analysed_file;
use crate::extract::facts::{DefKind, NO_PARENT};
use crate::index::{FileId, Index, NodeId};
use crate::intern::{FxHashMap, hash_bytes};
use crate::reach::BitSet;

/// Why a report could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error("{0}")]
    Read(#[from] std::io::Error),
    #[error("invalid JSON at byte {offset}: expected {what}")]
    Json { offset: usize, what: &'static str },
    #[error("not a coverage.py JSON report (no `files` object; create one with `coverage json`)")]
    NotCoverage,
}

impl From<json::JsonError> for CoverageError {
    fn from(e: json::JsonError) -> CoverageError {
        CoverageError::Json { offset: e.offset, what: e.what }
    }
}

/// Counters of one report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoverageStats {
    /// File entries in the report.
    pub report_files: u32,
    /// Entries mapped to an analysed file (directly or by suffix).
    pub matched: u32,
    /// Of `matched`: mapped by path suffix only.
    pub suffix_matched: u32,
    /// Matched files with a line number beyond their end (stale report).
    pub stale_files: u32,
    /// Defs and modules marked executed / not executed.
    pub executed: u32,
    pub not_executed: u32,
}

/// Coverage evidence per node of one index (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Coverage {
    executed: BitSet,
    not_executed: BitSet,
    stats: CoverageStats,
    /// First report path that matched no analysed file (for diagnostics).
    first_unmatched: Option<Vec<u8>>,
}

impl Coverage {
    /// Reads the report at `path` and maps it to `ix`. Relative report paths are tried against
    /// the report's directory, then `project_root`.
    pub fn load(ix: &Index, path: &Path, project_root: &Path) -> Result<Coverage, CoverageError> {
        let bytes = std::fs::read(path)?;
        let dir = path.parent().unwrap_or(Path::new(""));
        Coverage::parse(ix, &bytes, &[dir, project_root])
    }

    /// Maps the report `report` (coverage.py JSON) to `ix`; relative report paths are joined
    /// with each of `bases` in turn.
    pub fn parse(ix: &Index, report: &[u8], bases: &[&Path]) -> Result<Coverage, CoverageError> {
        let n = ix.node_count();
        let mut b = Builder {
            ix,
            paths: PathIndex::new(ix),
            cov: Coverage { executed: BitSet::new(n), not_executed: BitSet::new(n), ..Coverage::default() },
            state: Vec::new(),
            agg: Vec::new(),
        };
        let mut r = json::Reader::new(report);
        let (mut key, mut first) = (Vec::new(), true);
        let mut saw_files = false;
        r.begin_object()?;
        while r.next_key(&mut first, &mut key)? {
            if key == b"files" && r.peek() == Some(b'{') {
                saw_files = true;
                b.files(&mut r, bases)?;
            } else {
                r.skip_value()?;
            }
        }
        r.end()?;
        if !saw_files {
            return Err(CoverageError::NotCoverage);
        }
        let mut cov = b.cov;
        // Executed wins over a contradicting entry for the same file.
        for v in cov.executed.ones() {
            cov.not_executed.remove(v);
        }
        cov.stats.executed = cov.executed.count();
        cov.stats.not_executed = cov.not_executed.count();
        Ok(cov)
    }

    /// Whether the report proves that `v`'s own code ran (see the module docs).
    #[inline]
    pub fn executed(&self, v: NodeId) -> bool {
        v < self.executed.len() && self.executed.contains(v)
    }

    /// Whether the report proves that `v`'s own code never ran.
    #[inline]
    pub fn not_executed(&self, v: NodeId) -> bool {
        v < self.not_executed.len() && self.not_executed.contains(v)
    }

    /// Executed nodes, ascending.
    pub fn executed_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.executed.ones()
    }

    /// Nodes never executed, ascending.
    pub fn not_executed_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.not_executed.ones()
    }

    /// Number of nodes of the index this was built for.
    pub fn node_count(&self) -> u32 {
        self.executed.len()
    }

    pub fn stats(&self) -> &CoverageStats {
        &self.stats
    }

    /// The first report path that matched no analysed file, if any.
    pub fn first_unmatched(&self) -> Option<&[u8]> {
        self.first_unmatched.as_deref()
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.executed.heap_bytes() + self.not_executed.heap_bytes() + self.first_unmatched.as_ref().map_or(0, Vec::capacity)
    }
}

/// Per-def state while a file is applied.
const UNKNOWN: u8 = 0;
const EXECUTED: u8 = 1;
const NOT_EXECUTED: u8 = 2;
/// Per-class aggregate of its direct children: some child is not executed / some other state.
const HAS_NOT: u8 = 1;
const HAS_OTHER: u8 = 2;

struct Builder<'a> {
    ix: &'a Index,
    paths: PathIndex<'a>,
    cov: Coverage,
    /// Per def row of the current file: state, and class aggregate (reused).
    state: Vec<u8>,
    agg: Vec<u8>,
}

impl Builder<'_> {
    /// The `files` object.
    fn files(&mut self, r: &mut json::Reader<'_>, bases: &[&Path]) -> Result<(), CoverageError> {
        let (mut key, mut first) = (Vec::new(), true);
        let (mut field, mut exec, mut miss) = (Vec::new(), Vec::new(), Vec::new());
        let mut buf = Vec::new();
        r.begin_object()?;
        while r.next_key(&mut first, &mut key)? {
            self.cov.stats.report_files += 1;
            let file = self.paths.resolve(&key, bases, &mut buf);
            match file {
                Some((_, by_suffix)) => {
                    self.cov.stats.matched += 1;
                    self.cov.stats.suffix_matched += u32::from(by_suffix);
                }
                None if self.cov.first_unmatched.is_none() => self.cov.first_unmatched = Some(key.clone()),
                None => {}
            }
            if r.peek() != Some(b'{') {
                r.skip_value()?;
                continue;
            }
            exec.clear();
            miss.clear();
            let mut ffirst = true;
            r.begin_object()?;
            while r.next_key(&mut ffirst, &mut field)? {
                match &field[..] {
                    b"executed_lines" if file.is_some() => r.u32_array(&mut exec)?,
                    b"missing_lines" if file.is_some() => r.u32_array(&mut miss)?,
                    _ => r.skip_value()?,
                }
            }
            if let Some((f, _)) = file {
                self.apply(f, &mut exec, &mut miss);
            }
        }
        Ok(())
    }

    /// Marks the module and defs of file `f` from its executed and missing lines.
    fn apply(&mut self, f: FileId, exec: &mut [u32], miss: &mut [u32]) {
        for lines in [&mut *exec, &mut *miss] {
            if !lines.is_sorted() {
                lines.sort_unstable();
            }
        }
        let (exec, miss) = (&*exec, &*miss);
        let ix = self.ix;
        let ff = ix.file_facts(f);
        let m = ix.file_node(f);
        let beyond = |l: &[u32]| l.last().is_some_and(|&x| x > ff.lines);
        if beyond(exec) || beyond(miss) {
            self.cov.stats.stale_files += 1;
        }
        if !exec.is_empty() {
            self.cov.executed.insert(m);
        } else if !miss.is_empty() {
            self.cov.not_executed.insert(m);
        }
        let defs = &ix.facts().defs;
        let len = ff.defs.len as usize;
        self.state.clear();
        self.state.resize(len, UNKNOWN);
        self.agg.clear();
        self.agg.resize(len, 0);
        // Children follow their parent (pre-order): reverse order sees every child of a class
        // before the class.
        for d in (1..len).rev() {
            let row = ff.defs.start as usize + d;
            let (start, name, end) = (defs.start_line[row], defs.name_line[row], defs.end_line[row]);
            let header_missing = any_in(miss, start, name);
            let has_body = end > name;
            let s = match DefKind::from_u8(defs.kind[row]) {
                DefKind::Class if header_missing || self.agg[d] == HAS_NOT => NOT_EXECUTED,
                DefKind::Class | DefKind::Module => UNKNOWN,
                DefKind::Function if has_body && any_in(exec, name + 1, end) => EXECUTED,
                DefKind::Function if header_missing || (has_body && any_in(miss, name + 1, end)) => NOT_EXECUTED,
                DefKind::Function => UNKNOWN,
            };
            self.state[d] = s;
            let p = defs.parent[row];
            if p != NO_PARENT && (p as usize) < d && DefKind::from_u8(defs.kind[ff.defs.start as usize + p as usize]) == DefKind::Class {
                self.agg[p as usize] |= if s == NOT_EXECUTED { HAS_NOT } else { HAS_OTHER };
            }
            match s {
                EXECUTED => self.cov.executed.insert(m + d as u32),
                NOT_EXECUTED => self.cov.not_executed.insert(m + d as u32),
                _ => {}
            }
        }
    }
}

/// Whether sorted `lines` has a value in `lo..=hi`.
#[inline]
fn any_in(lines: &[u32], lo: u32, hi: u32) -> bool {
    let i = lines.partition_point(|&x| x < lo);
    i < lines.len() && lines[i] <= hi
}

/// Report path -> analysed file.
struct PathIndex<'a> {
    ix: &'a Index,
    exact: FxHashMap<&'a [u8], FileId>,
    /// (hash of the file name, file), sorted; built on the first suffix lookup.
    by_name: Option<Vec<(u64, FileId)>>,
    /// Base directory joined with a relative report path (reused).
    joined: Vec<u8>,
}

impl<'a> PathIndex<'a> {
    fn new(ix: &'a Index) -> PathIndex<'a> {
        let mut exact = FxHashMap::default();
        for f in 0..ix.file_count() {
            if analysed_file(ix, f) {
                exact.insert(path_bytes(ix, f), f);
            }
        }
        PathIndex { ix, exact, by_name: None, joined: Vec::new() }
    }

    /// The file of report path `key`, and whether it was found by suffix only. `buf` is
    /// scratch space.
    fn resolve(&mut self, key: &[u8], bases: &[&Path], buf: &mut Vec<u8>) -> Option<(FileId, bool)> {
        if key.first() == Some(&b'/') {
            normalize_into(key, buf);
            if let Some(&f) = self.exact.get(&buf[..]) {
                return Some((f, false));
            }
        } else {
            let joined = &mut self.joined;
            for base in bases {
                joined.clear();
                joined.extend_from_slice(base.as_os_str().as_encoded_bytes());
                joined.push(b'/');
                joined.extend_from_slice(key);
                normalize_into(joined, buf);
                if let Some(&f) = self.exact.get(&buf[..]) {
                    return Some((f, false));
                }
            }
        }
        self.by_suffix(key).map(|f| (f, true))
    }

    /// The unique analysed file sharing the longest path suffix with `key`: at least two
    /// components (directory and name), or the whole of a one-component key. `\` separators
    /// (reports made on Windows) count as `/`.
    fn by_suffix(&mut self, key: &[u8]) -> Option<FileId> {
        let ix = self.ix;
        let sep = |c: &u8| *c == b'/' || *c == b'\\';
        let comps = key.split(sep).filter(|c| !c.is_empty() && *c != b".");
        let name = comps.clone().next_back()?;
        let need = comps.clone().count().min(2);
        let by_name = self.by_name.get_or_insert_with(|| {
            let mut v: Vec<(u64, FileId)> = Vec::new();
            for f in 0..ix.file_count() {
                if analysed_file(ix, f) {
                    let p = path_bytes(ix, f);
                    let base = p.rsplit(|&c| c == b'/').next().unwrap_or(p);
                    v.push((hash_bytes(base), f));
                }
            }
            v.sort_unstable();
            v
        });
        let h = hash_bytes(name);
        let lo = by_name.partition_point(|&(x, _)| x < h);
        let (mut best, mut best_n, mut tie) = (None, 0usize, false);
        for &(x, f) in &by_name[lo..] {
            if x != h {
                break;
            }
            let path = path_bytes(ix, f);
            let n = comps.clone().rev().zip(path.split(|&c| c == b'/').rev()).take_while(|(a, b)| a == b).count();
            if n > best_n {
                (best, best_n, tie) = (Some(f), n, false);
            } else if n == best_n && n > 0 {
                tie = true;
            }
        }
        if tie || best_n < need { None } else { best }
    }
}

#[inline]
fn path_bytes(ix: &Index, f: FileId) -> &[u8] {
    ix.file_path(f).as_os_str().as_encoded_bytes()
}

/// `path` with `.` and empty components removed and `..` applied lexically, into `out`
/// (cleared first). An absolute path stays absolute; `/..` is `/`.
fn normalize_into(path: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let abs = path.first() == Some(&b'/');
    if abs {
        out.push(b'/');
    }
    let floor = out.len();
    for c in path.split(|&c| c == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                // Pop the last component unless there is none (or it is itself `..`).
                let last = out[floor..].rsplit(|&c| c == b'/').next().unwrap_or(b"");
                if out.len() > floor && last != b".." {
                    let cut = out[floor..].iter().rposition(|&c| c == b'/').map_or(floor, |i| floor + i);
                    out.truncate(cut);
                } else if !abs {
                    if out.len() > floor {
                        out.push(b'/');
                    }
                    out.extend_from_slice(b"..");
                }
            }
            _ => {
                if out.len() > floor {
                    out.push(b'/');
                }
                out.extend_from_slice(c);
            }
        }
    }
}
