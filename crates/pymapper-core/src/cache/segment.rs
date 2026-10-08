//! Facts segments: the extracted facts and metrics of a set of files, in one cache file.
//!
//! ```text
//!   section          type   rows
//!   STR_OFFS         u32    strings + 1   local string table (ids are local: raw = index + 1)
//!   STR_BYTES        u8     -
//!   PATH_OFFS        u32    files + 1     paths relative to the segment base (or absolute)
//!   PATH_BYTES       u8     -
//!   FILE_HDR         u32    files * 30    FileFacts (13 segment-absolute runs, lines, flags),
//!                                         first metrics row, reserved
//!   FILE_STAMP       u64    files * 3     size, mtime (ns), content hash
//!   16..             ...    one section per fact column, then per metrics column
//! ```
//!
//! Every fact column is stored as its in-memory column: plain columns are memcpy'd both ways,
//! `Sym` columns carry local string ids. A reader copies a file's rows into a [`FactStore`]
//! unchanged (local ids included); the loader then remaps them with the segment's
//! [`Interner`] like any worker's ids. Nothing is parsed: opening a segment validates it and
//! builds a path lookup, copying a file is a few `extend_from_slice` per column.

use std::io::{self, Seek, Write};
use std::path::Path;

use super::format::{CacheError, Reader, Writer};
use super::mmap::Pod;
use crate::extract::MetricsTable;
use crate::extract::facts::{FactStore, FileFacts, NO_PARENT, Run, TableBases};
use crate::intern::{FxHashMap, Interner, Sym, hash_bytes};

pub(crate) const STR_OFFS: u32 = 0;
pub(crate) const STR_BYTES: u32 = 1;
pub(crate) const PATH_OFFS: u32 = 2;
pub(crate) const PATH_BYTES: u32 = 3;
pub(crate) const FILE_HDR: u32 = 4;
pub(crate) const FILE_STAMP: u32 = 5;
/// `u32`s per file in `FILE_HDR`.
const HDR: usize = 30;

/// What the loader knows about a source file when it was cached: enough to tell, without
/// reading it, that it is unchanged (size + mtime), and failing that, by content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// Modification time in nanoseconds since the epoch; 0 = unknown (forces a content
    /// check).
    pub mtime_ns: u64,
    /// [`hash_bytes`] of the content.
    pub hash: u64,
}

/// One file to encode: its path (as stored), header (store-absolute runs), first metrics row
/// and stamp.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EncFile<'a> {
    pub path: &'a [u8],
    pub ff: FileFacts,
    pub metrics: u32,
    pub stamp: Stamp,
}

/// Global `Sym` -> local id (raw, index + 1) while encoding one segment. Reused across
/// segments; sized by the segment's strings, not the global interner's.
#[derive(Default)]
pub(crate) struct SymMap {
    ids: FxHashMap<Sym, u32>,
    order: Vec<Sym>,
}

impl SymMap {
    fn begin(&mut self) {
        self.ids.clear();
        self.order.clear();
    }

    #[inline]
    fn local(&mut self, s: Sym) -> u32 {
        let next = self.order.len() as u32 + 1;
        let id = *self.ids.entry(s).or_insert(next);
        if id == next {
            self.order.push(s);
        }
        id
    }
}

/// Every column of a [`FactStore`] as `id: column @ run-of-FileFacts => kind`:
///
/// - `P`: plain scalars;
/// - `S`: `Sym`; `O`: `Option<Sym>` (raw ids, 0 = `None`);
/// - `C(len column)`: a chain column (an id only where the length is 1, see `ChainCol`).
macro_rules! fact_columns {
    ($cb:ident!($($args:tt)*)) => {
        $cb!($($args)*;
            16: defs.kind @ defs => P,
            17: defs.flags @ defs => P,
            18: defs.name @ defs => O,
            19: defs.parent @ defs => P,
            20: defs.start @ defs => P,
            21: defs.end @ defs => P,
            22: defs.start_line @ defs => P,
            23: defs.end_line @ defs => P,
            24: defs.name_line @ defs => P,
            25: defs.name_col @ defs => P,
            26: defs.decorators_len @ defs => P,
            27: defs.bases_len @ defs => P,
            28: defs.params @ defs => P,
            29: defs.params_len @ defs => P,
            32: decorators.owner @ decorators => P,
            33: decorators.chain @ decorators => C(decorators.chain_len),
            34: decorators.chain_len @ decorators => P,
            35: decorators.flags @ decorators => P,
            40: bases.owner @ bases => P,
            41: bases.chain @ bases => C(bases.chain_len),
            42: bases.chain_len @ bases => P,
            43: bases.keyword @ bases => O,
            44: bases.flags @ bases => P,
            48: params.name @ params => S,
            49: params.flags @ params => P,
            56: imports.scope @ imports => P,
            57: imports.module @ imports => C(imports.module_len),
            58: imports.module_len @ imports => P,
            59: imports.name @ imports => O,
            60: imports.alias @ imports => O,
            61: imports.level @ imports => P,
            62: imports.flags @ imports => P,
            63: imports.line @ imports => P,
            64: imports.col @ imports => P,
            72: calls.scope @ calls => P,
            73: calls.chain @ calls => C(calls.chain_len),
            74: calls.chain_len @ calls => P,
            75: calls.flags @ calls => P,
            76: calls.line @ calls => P,
            77: calls.col @ calls => P,
            80: bindings.scope @ bindings => P,
            81: bindings.target @ bindings => C(bindings.target_len),
            82: bindings.target_len @ bindings => P,
            83: bindings.kind @ bindings => P,
            84: bindings.flags @ bindings => P,
            85: bindings.value_chain @ bindings => C(bindings.value_len),
            86: bindings.value_len @ bindings => P,
            87: bindings.line @ bindings => P,
            88: decls.scope @ decls => P,
            89: decls.kind @ decls => P,
            90: decls.name @ decls => S,
            96: names @ names => S,
            97: exports @ exports => S,
            98: evidence @ evidence => P,
            104: diags.kind @ diags => P,
            105: diags.line @ diags => P,
            106: diags.col @ diags => P,
            112: suppressions.line @ suppressions => P,
            113: suppressions.codes @ suppressions => R,
            115: suppressions.all @ suppressions => P,
        )
    };
}

/// Metrics columns (rows: one per def row except `<module>`).
macro_rules! metric_columns {
    ($cb:ident!($($args:tt)*)) => {
        $cb!($($args)*;
            128: cyclomatic,
            129: cognitive,
            130: max_nesting,
            131: loc,
            132: distinct_operators,
            133: distinct_operands,
            134: operators,
            135: operands,
            136: params,
        )
    };
}

/// Compile-time guard: exhaustive patterns (no `..`) over every table, so adding, removing or
/// renaming a column of [`FactStore`] or [`MetricsTable`] fails to build here until
/// `fact_columns!` / `metric_columns!` (and, when the extractor's output changes,
/// [`super::FACTS_VERSION`]) are updated. A column missing from those lists would otherwise
/// be silently dropped by the cache and leave copied tables ragged.
#[allow(dead_code)]
fn every_column_is_listed(s: &FactStore, m: &MetricsTable) {
    const _: () = assert!(HDR == 13 * 2 + 4, "FILE_HDR: 13 runs, lines, flags, metrics row, reserved");
    use crate::extract::facts::{Bases, Bindings, Calls, Decls, Decorators, Defs, Diags, Imports, Params, Suppressions};
    let FactStore {
        defs:
            Defs {
                kind: _,
                flags: _,
                name: _,
                parent: _,
                start: _,
                end: _,
                start_line: _,
                end_line: _,
                name_line: _,
                name_col: _,
                decorators_len: _,
                bases_len: _,
                params: _,
                params_len: _,
            },
        decorators: Decorators { owner: _, chain: _, chain_len: _, flags: _ },
        bases: Bases { owner: _, chain: _, chain_len: _, keyword: _, flags: _ },
        params: Params { name: _, flags: _ },
        imports: Imports { scope: _, module: _, module_len: _, name: _, alias: _, level: _, flags: _, line: _, col: _ },
        calls: Calls { scope: _, chain: _, chain_len: _, flags: _, line: _, col: _ },
        bindings:
            Bindings { scope: _, target: _, target_len: _, kind: _, flags: _, value_chain: _, value_len: _, line: _ },
        decls: Decls { scope: _, kind: _, name: _ },
        names: _,
        exports: _,
        evidence: _,
        diags: Diags { kind: _, line: _, col: _ },
        suppressions: Suppressions { line: _, codes: _, all: _ },
        // The extractor's private scratch. New tables are caught by `TableBases` below.
        ..
    } = s;
    let TableBases {
        defs: _,
        decorators: _,
        bases: _,
        params: _,
        imports: _,
        calls: _,
        bindings: _,
        decls: _,
        names: _,
        exports: _,
        evidence: _,
        diags: _,
        suppressions: _,
    } = s.bases();
    // `for_runs!` and the `FILE_HDR` layout (`HDR` words).
    let FileFacts {
        defs: _,
        decorators: _,
        bases: _,
        params: _,
        imports: _,
        calls: _,
        bindings: _,
        decls: _,
        names: _,
        exports: _,
        evidence: _,
        diags: _,
        suppressions: _,
        lines: _,
        flags: _,
    } = FileFacts::default();
    let MetricsTable {
        cyclomatic: _,
        cognitive: _,
        max_nesting: _,
        loc: _,
        distinct_operators: _,
        distinct_operands: _,
        operators: _,
        operands: _,
        params: _,
    } = m;
}

/// Second section of a `Run` column (`id` holds the starts, `id + 1` the lengths).
const fn run_len_id(id: u32) -> u32 {
    id + 1
}

/// Section id of a chain length column.
macro_rules! len_id_of {
    (decorators.chain_len) => { 34 };
    (bases.chain_len) => { 42 };
    (imports.module_len) => { 58 };
    (calls.chain_len) => { 74 };
    (bindings.target_len) => { 82 };
    (bindings.value_len) => { 86 };
}

/// Metrics rows of a file: one per def row except `<module>`.
#[inline]
fn metrics_run(ff: &FileFacts, first: u32) -> std::ops::Range<usize> {
    first as usize..(first + ff.defs.len.saturating_sub(1)) as usize
}

/// Applies `$m!(field)` to every run of [`FileFacts`], in header order.
macro_rules! for_runs {
    ($m:ident) => {
        $m!(defs);
        $m!(decorators);
        $m!(bases);
        $m!(params);
        $m!(imports);
        $m!(calls);
        $m!(bindings);
        $m!(decls);
        $m!(names);
        $m!(exports);
        $m!(evidence);
        $m!(diags);
        $m!(suppressions);
    };
}

fn put_hdr(ff: &FileFacts, metrics: u32, out: &mut Vec<u32>) {
    macro_rules! put {
        ($f:ident) => {
            out.push(ff.$f.start);
            out.push(ff.$f.len);
        };
    }
    for_runs!(put);
    out.extend_from_slice(&[ff.lines, u32::from(ff.flags), metrics, 0]);
}

/// Header and first metrics row of a file from its `HDR` words.
fn get_hdr(h: &[u32]) -> (FileFacts, u32) {
    let mut ff = FileFacts::default();
    let mut i = 0;
    macro_rules! get {
        ($f:ident) => {
            ff.$f = Run { start: h[i], len: h[i + 1] };
            i += 2;
        };
    }
    for_runs!(get);
    ff.lines = h[i];
    ff.flags = h[i + 1] as u8;
    (ff, h[i + 2])
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

macro_rules! enc_columns {
    ($w:ident, $store:ident, $files:ident, $map:ident;
     $($id:literal : $($col:ident).+ @ $run:ident => $kind:ident $(($($len:ident).+))?),+ $(,)?) => {
        $( enc_one!($kind $(($($len).+))?; $w, $id, $store, $files, $map, [$($col).+], $run); )+
    };
}

macro_rules! enc_one {
    (P; $w:ident, $id:literal, $store:ident, $files:ident, $map:ident, [$($col:ident).+], $run:ident) => {
        $w.put_with($id, |b| {
            for f in $files {
                b.extend_from_slice(super::mmap::as_bytes(&$store.$($col).+[f.ff.$run.range()]));
            }
        });
    };
    (S; $w:ident, $id:literal, $store:ident, $files:ident, $map:ident, [$($col:ident).+], $run:ident) => {
        $w.put_with($id, |b| {
            for f in $files {
                for &s in &$store.$($col).+[f.ff.$run.range()] {
                    b.extend_from_slice(&$map.local(s).to_le_bytes());
                }
            }
        });
    };
    (O; $w:ident, $id:literal, $store:ident, $files:ident, $map:ident, [$($col:ident).+], $run:ident) => {
        $w.put_with($id, |b| {
            for f in $files {
                for &s in &$store.$($col).+[f.ff.$run.range()] {
                    let v = s.map_or(0, |s| $map.local(s));
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
        });
    };
    (C($($len:ident).+); $w:ident, $id:literal, $store:ident, $files:ident, $map:ident, [$($col:ident).+], $run:ident) => {
        $w.put_with($id, |b| {
            for f in $files {
                let r = f.ff.$run.range();
                for (&c, &l) in $store.$($col).+[r.clone()].iter().zip(&$store.$($len).+[r]) {
                    let v = if l == 1 { $map.local(c) } else { c.to_raw() };
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
        });
    };
    (R; $w:ident, $id:literal, $store:ident, $files:ident, $map:ident, [$($col:ident).+], $run:ident) => {
        $w.put_with($id, |b| {
            for f in $files {
                for r in &$store.$($col).+[f.ff.$run.range()] {
                    b.extend_from_slice(&r.start.to_le_bytes());
                }
            }
        });
        $w.put_with(run_len_id($id), |b| {
            for f in $files {
                for r in &$store.$($col).+[f.ff.$run.range()] {
                    b.extend_from_slice(&r.len.to_le_bytes());
                }
            }
        });
    };
}

macro_rules! enc_metrics {
    ($w:ident, $metrics:ident, $files:ident; $($id:literal : $col:ident),+ $(,)?) => {
        $(
            $w.put_with($id, |b| {
                for f in $files {
                    b.extend_from_slice(super::mmap::as_bytes(&$metrics.$col[metrics_run(&f.ff, f.metrics)]));
                }
            });
        )+
    };
}

/// Encodes the facts of `files` (rows of `store`/`metrics`, ids of `strings`) as one segment
/// file, streamed to `out`; returns `out` and the file length. Strings are renumbered densely
/// in first-use order, so the segment holds only the strings its files use. Memory: the
/// largest column of the segment.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode<W: Write + Seek>(
    strings: &Interner,
    store: &FactStore,
    metrics: &MetricsTable,
    files: &[EncFile<'_>],
    map: &mut SymMap,
    kind: u32,
    schema: u64,
    key: u64,
    out: W,
) -> io::Result<(W, u64)> {
    let mut w = Writer::new(out);
    map.begin();
    fact_columns!(enc_columns!(w, store, files, map));
    metric_columns!(enc_metrics!(w, metrics, files));

    // Headers rebased onto the segment's own tables.
    let mut hdr = Vec::with_capacity(files.len() * HDR);
    let mut next = FileFacts::default();
    let mut metrics_next = 0u32;
    for f in files {
        let mut ff = f.ff;
        macro_rules! rebase {
            ($t:ident) => {
                ff.$t.start = next.$t.start;
                next.$t.start += ff.$t.len;
            };
        }
        for_runs!(rebase);
        put_hdr(&ff, metrics_next, &mut hdr);
        metrics_next += ff.defs.len.saturating_sub(1);
    }
    w.put(FILE_HDR, &hdr);
    drop(hdr);
    let stamps: Vec<u64> = files.iter().flat_map(|f| [f.stamp.size, f.stamp.mtime_ns, f.stamp.hash]).collect();
    w.put(FILE_STAMP, &stamps);
    drop(stamps);

    let mut offs = Vec::with_capacity(files.len() + 1);
    offs.push(0u32);
    w.put_with(PATH_BYTES, |b| {
        let start = b.len();
        for f in files {
            b.extend_from_slice(f.path);
            offs.push((b.len() - start) as u32);
        }
    });
    w.put(PATH_OFFS, &offs);

    offs.clear();
    offs.reserve(map.order.len() + 1);
    offs.push(0);
    w.put_with(STR_BYTES, |b| {
        let start = b.len();
        for &s in &map.order {
            b.extend_from_slice(strings.resolve(s));
            offs.push((b.len() - start) as u32);
        }
    });
    w.put(STR_OFFS, &offs);
    w.finish(kind, schema, key)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// An open, validated facts segment.
pub(crate) struct Segment {
    r: Reader,
    files: u32,
    strings: u32,
    /// Path hash -> file. A path whose hash collides with an earlier one is not found (it is
    /// re-extracted), so lookups compare the bytes.
    by_path: FxHashMap<u64, u32>,
}

macro_rules! copy_columns {
    ($seg:ident, $store:ident, $map:ident, $sff:ident;
     $($id:literal : $($col:ident).+ @ $run:ident => $kind:ident $(($($len:ident).+))?),+ $(,)?) => {
        $( copy_one!($kind $(($($len).+))?; $seg, $id, $store, $map, [$($col).+], $sff.$run.range()); )+
    };
}

macro_rules! copy_one {
    (P; $seg:ident, $id:literal, $store:ident, $map:ident, [$($col:ident).+], $r:expr) => {
        $store.$($col).+.extend_from_slice(&$seg.col($id)[$r]);
    };
    (S; $seg:ident, $id:literal, $store:ident, $map:ident, [$($col:ident).+], $r:expr) => {
        $store.$($col).+.extend($seg.col::<u32>($id)[$r].iter().map(|&v| $map(v)));
    };
    (O; $seg:ident, $id:literal, $store:ident, $map:ident, [$($col:ident).+], $r:expr) => {
        $store.$($col).+.extend($seg.col::<u32>($id)[$r].iter().map(|&v| (v != 0).then(|| $map(v))));
    };
    (C($($len:ident).+); $seg:ident, $id:literal, $store:ident, $map:ident, [$($col:ident).+], $r:expr) => {
        let r = $r;
        let lens = &$seg.col::<u8>(len_id_of!($($len).+))[r.clone()];
        $store.$($col).+.extend($seg.col::<u32>($id)[r].iter().zip(lens).map(|(&v, &l)| if l == 1 { $map(v) } else { sym(v) }));
    };
    (R; $seg:ident, $id:literal, $store:ident, $map:ident, [$($col:ident).+], $r:expr) => {
        let (s, l) = ($seg.col::<u32>($id), $seg.col::<u32>(run_len_id($id)));
        let r = $r;
        $store.$($col).+.extend(s[r.clone()].iter().zip(&l[r]).map(|(&start, &len)| Run { start, len }));
    };
}

macro_rules! copy_metrics {
    ($seg:ident, $metrics:ident, $r:ident; $($id:literal : $col:ident),+ $(,)?) => {
        $( $metrics.$col.extend_from_slice(&$seg.col($id)[$r.clone()]); )+
    };
}

/// A validated non-zero raw id.
#[inline]
fn sym(v: u32) -> Sym {
    Sym::from_raw(v).unwrap_or(Sym::from_index(0))
}

impl Segment {
    /// Opens and validates the segment at `path` (see [`Segment::from_reader`]).
    pub fn open(path: &Path, kind: u32, schema: u64, key: u64) -> Result<Segment, CacheError> {
        Segment::from_reader(Reader::open(path, kind, schema, key)?)
    }

    /// Validates every structural invariant the copy and the loader rely on (string table,
    /// runs in bounds, ids in range, chains inside their file's names, parents, owners and
    /// param runs inside their file), so that copying never panics; then indexes the paths.
    pub fn from_reader(r: Reader) -> Result<Segment, CacheError> {
        let strings = offsets(r.get::<u32>(STR_OFFS)?, r.bytes(STR_BYTES).len())?;
        let files = offsets(r.get::<u32>(PATH_OFFS)?, r.bytes(PATH_BYTES).len())?;
        let mut seg = Segment { r, files, strings, by_path: FxHashMap::default() };
        if seg.r.get::<u32>(FILE_HDR)?.len() != files as usize * HDR
            || seg.r.get::<u64>(FILE_STAMP)?.len() != files as usize * 3
        {
            return Err(CacheError::Corrupt("file table"));
        }
        seg.validate()?;
        seg.by_path.reserve(files as usize);
        for i in 0..files {
            seg.by_path.entry(hash_bytes(seg.path(i))).or_insert(i);
        }
        Ok(seg)
    }

    /// Column `id` (validated at open; empty if absent, which validation rejects for files
    /// with rows).
    #[inline]
    fn col<T: Pod>(&self, id: u32) -> &[T] {
        self.r.get(id).unwrap_or(&[])
    }

    #[inline]
    pub fn file_count(&self) -> u32 {
        self.files
    }

    /// Size of the cache file in bytes.
    pub fn byte_len(&self) -> usize {
        self.r.len()
    }

    /// Stored path of file `i`.
    #[inline]
    pub fn path(&self, i: u32) -> &[u8] {
        let offs = self.col::<u32>(PATH_OFFS);
        &self.r.bytes(PATH_BYTES)[offs[i as usize] as usize..offs[i as usize + 1] as usize]
    }

    #[inline]
    pub fn stamp(&self, i: u32) -> Stamp {
        let s = &self.col::<u64>(FILE_STAMP)[i as usize * 3..i as usize * 3 + 3];
        Stamp { size: s[0], mtime_ns: s[1], hash: s[2] }
    }

    /// Header (segment-absolute runs) and first metrics row of file `i`.
    #[inline]
    fn hdr(&self, i: u32) -> (FileFacts, u32) {
        get_hdr(&self.col::<u32>(FILE_HDR)[i as usize * HDR..(i as usize + 1) * HDR])
    }

    /// The file stored under `path`.
    #[inline]
    pub fn find(&self, path: &[u8]) -> Option<u32> {
        let i = *self.by_path.get(&hash_bytes(path))?;
        (self.path(i) == path).then_some(i)
    }

    /// String `i` of the local table.
    #[inline]
    pub(crate) fn string(&self, i: usize) -> &[u8] {
        let offs = self.col::<u32>(STR_OFFS);
        &self.r.bytes(STR_BYTES)[offs[i] as usize..offs[i + 1] as usize]
    }

    /// Number of strings in the local table.
    #[inline]
    pub fn string_count(&self) -> u32 {
        self.strings
    }

    /// Appends the rows of file `i` to `store` / `metrics` and returns its header,
    /// store-absolute. Local string ids become ids of `strings` through `remap` (local index
    /// -> raw `Sym`, 0 = not yet interned; `string_count` entries), filled on first use, so
    /// each string of the segment is hashed at most once per destination interner.
    pub fn copy_file(
        &self,
        i: u32,
        store: &mut FactStore,
        metrics: &mut MetricsTable,
        strings: &mut Interner,
        remap: &mut [u32],
    ) -> FileFacts {
        debug_assert_eq!(remap.len(), self.strings as usize);
        let (sff, m0) = self.hdr(i);
        let b = store.bases();
        let mut map = |v: u32| -> Sym {
            let k = (v - 1) as usize;
            if remap[k] == 0 {
                remap[k] = strings.intern(self.string(k)).to_raw();
            }
            sym(remap[k])
        };
        fact_columns!(copy_columns!(self, store, map, sff));
        let mr = metrics_run(&sff, m0);
        metric_columns!(copy_metrics!(self, metrics, mr));
        let mut ff = sff;
        macro_rules! base {
            ($t:ident) => {
                ff.$t.start = b.$t;
            };
        }
        for_runs!(base);
        ff
    }
}

/// Validates an offsets column (non-empty, starts at 0, non-decreasing, ends at `bytes`);
/// returns the number of entries.
fn offsets(offs: &[u32], bytes: usize) -> Result<u32, CacheError> {
    let ok = offs.first() == Some(&0)
        && offs.last().is_some_and(|&l| l as usize == bytes)
        && offs.windows(2).all(|w| w[0] <= w[1]);
    if !ok {
        return Err(CacheError::Corrupt("offsets"));
    }
    u32::try_from(offs.len() - 1).map_err(|_| CacheError::Corrupt("offsets"))
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

macro_rules! val_columns {
    ($seg:ident, $end:ident, $n:ident;
     $($id:literal : $($col:ident).+ @ $run:ident => $kind:ident $(($($len:ident).+))?),+ $(,)?) => {
        $( val_one!($kind $(($($len).+))?; $seg, $id, $end.$run.start as usize, $n, [$($col).+]); )+
    };
}

macro_rules! val_one {
    (P; $seg:ident, $id:literal, $rows:expr, $n:ident, [$($col:ident).+]) => {
        if !$seg.plain_ok($id, $rows, |s: &FactStore| &s.$($col).+) {
            return Err(CacheError::Corrupt("column length"));
        }
    };
    (S; $seg:ident, $id:literal, $rows:expr, $n:ident, [$($col:ident).+]) => {
        let c = $seg.r.get::<u32>($id)?;
        if c.len() < $rows || !c.iter().all(|&v| v != 0 && v <= $n) {
            return Err(CacheError::Corrupt("string id"));
        }
    };
    (O; $seg:ident, $id:literal, $rows:expr, $n:ident, [$($col:ident).+]) => {
        let c = $seg.r.get::<u32>($id)?;
        if c.len() < $rows || !c.iter().all(|&v| v <= $n) {
            return Err(CacheError::Corrupt("string id"));
        }
    };
    (C($($len:ident).+); $seg:ident, $id:literal, $rows:expr, $n:ident, [$($col:ident).+]) => {
        let c = $seg.r.get::<u32>($id)?;
        let lens = $seg.r.get::<u8>(len_id_of!($($len).+))?;
        if c.len() < $rows || lens.len() < $rows || !c.iter().zip(lens).all(|(&v, &l)| v != 0 && (l != 1 || v <= $n)) {
            return Err(CacheError::Corrupt("chain"));
        }
    };
    (R; $seg:ident, $id:literal, $rows:expr, $n:ident, [$($col:ident).+]) => {
        if $seg.r.get::<u32>($id)?.len() < $rows || $seg.r.get::<u32>(run_len_id($id))?.len() < $rows {
            return Err(CacheError::Corrupt("column length"));
        }
    };
}

macro_rules! val_metrics {
    ($seg:ident, $rows:ident; $($id:literal : $col:ident),+ $(,)?) => {
        $(
            if !$seg.plain_ok($id, $rows, |m: &MetricsTable| &m.$col) {
                return Err(CacheError::Corrupt("metrics length"));
            }
        )+
    };
}

impl Segment {
    /// Plain column `id` has at least `rows` elements of the type of `_col`'s column.
    fn plain_ok<S, T: Pod>(&self, id: u32, rows: usize, _col: fn(&S) -> &Vec<T>) -> bool {
        self.r.get::<T>(id).is_ok_and(|c| c.len() >= rows)
    }

    fn validate(&self) -> Result<(), CacheError> {
        let n = self.strings;
        // Rows each column must have: the furthest run end of any file.
        let mut end = FileFacts::default();
        let mut m_end = 0usize;
        for i in 0..self.files {
            let (ff, m0) = self.hdr(i);
            if ff.defs.len == 0 {
                return Err(CacheError::Corrupt("file without a <module> row"));
            }
            macro_rules! max_end {
                ($t:ident) => {
                    let e = ff.$t.start.checked_add(ff.$t.len).ok_or(CacheError::Corrupt("run"))?;
                    end.$t.start = end.$t.start.max(e);
                };
            }
            for_runs!(max_end);
            let me = m0.checked_add(ff.defs.len - 1).ok_or(CacheError::Corrupt("metrics run"))?;
            m_end = m_end.max(me as usize);
        }
        fact_columns!(val_columns!(self, end, n));
        metric_columns!(val_metrics!(self, m_end));
        for i in 0..self.files {
            self.validate_file(&self.hdr(i).0)?;
        }
        Ok(())
    }

    /// File-relative references of one file stay inside the file.
    fn validate_file(&self, ff: &FileFacts) -> Result<(), CacheError> {
        let defs = ff.defs.len;
        let names = u64::from(ff.names.len);
        let bad = |what| Err(CacheError::Corrupt(what));
        let d = ff.defs.range();
        if !self.typed(19, |s| &s.defs.parent)[d.clone()].iter().all(|&p| p == NO_PARENT || p < defs) {
            return bad("parent");
        }
        let (pstart, plen) = (&self.typed(28, |s| &s.defs.params)[d.clone()], &self.typed(29, |s| &s.defs.params_len)[d]);
        if !pstart.iter().zip(plen).all(|(&s, &l)| u64::from(s) + u64::from(l) <= u64::from(ff.params.len)) {
            return bad("params");
        }
        let dec_count = self.typed(26, |s| &s.defs.decorators_len);
        owned(self.typed(32, |s| &s.decorators.owner), ff.decorators, dec_count, ff.defs)?;
        owned(self.typed(40, |s| &s.bases.owner), ff.bases, self.typed(27, |s| &s.defs.bases_len), ff.defs)?;
        let scopes = [
            (self.typed(56, |s| &s.imports.scope), ff.imports),
            (self.typed(72, |s| &s.calls.scope), ff.calls),
            (self.typed(80, |s| &s.bindings.scope), ff.bindings),
            (self.typed(88, |s| &s.decls.scope), ff.decls),
        ];
        for (col, run) in scopes {
            if !col[run.range()].iter().all(|&s| s < defs) {
                return bad("scope");
            }
        }
        let chains = [
            (self.typed(33, |s| &s.decorators.chain), self.typed(34, |s| &s.decorators.chain_len), ff.decorators),
            (self.typed(41, |s| &s.bases.chain), self.typed(42, |s| &s.bases.chain_len), ff.bases),
            (self.typed(57, |s| &s.imports.module), self.typed(58, |s| &s.imports.module_len), ff.imports),
            (self.typed(73, |s| &s.calls.chain), self.typed(74, |s| &s.calls.chain_len), ff.calls),
            (self.typed(81, |s| &s.bindings.target), self.typed(82, |s| &s.bindings.target_len), ff.bindings),
            (self.typed(85, |s| &s.bindings.value_chain), self.typed(86, |s| &s.bindings.value_len), ff.bindings),
        ];
        for (col, lens, run) in chains {
            let r = run.range();
            let ok = col[r.clone()].iter().zip(&lens[r]).all(|(&c, &l)| l < 2 || u64::from(c) - 1 + u64::from(l) <= names);
            if !ok {
                return bad("chain");
            }
        }
        let r = ff.suppressions.range();
        let (cs, cl) = (&self.col::<u32>(113)[r.clone()], &self.col::<u32>(run_len_id(113))[r]);
        if !cs.iter().zip(cl).all(|(&s, &l)| u64::from(s) + u64::from(l) <= names) {
            return bad("suppression codes");
        }
        Ok(())
    }

    /// Column `id` read as the stored type of the [`FactStore`] column `_col` selects (`Sym`
    /// columns as their raw `u32` ids), so that a column changing type cannot be read with a
    /// stale element type here.
    #[inline]
    fn typed<T: Stored>(&self, id: u32, _col: fn(&FactStore) -> &Vec<T>) -> &[T::Raw] {
        self.col::<T::Raw>(id)
    }
}

/// How a [`FactStore`] column element is stored in a segment.
trait Stored {
    type Raw: Pod;
}

impl<T: Pod> Stored for T {
    type Raw = T;
}

impl Stored for Sym {
    type Raw = u32;
}

/// Rows owned by defs (`owner` column over `rows`, per-def counts over `defs`): owners are
/// non-decreasing defs of the file and each def owns exactly its count.
fn owned(owner: &[u32], rows: Run, counts: &[u8], defs: Run) -> Result<(), CacheError> {
    let owner = &owner[rows.range()];
    let mut i = 0usize;
    for (d, &n) in counts[defs.range()].iter().enumerate() {
        let n = usize::from(n);
        if i + n > owner.len() || !owner[i..i + n].iter().all(|&o| o as usize == d) {
            return Err(CacheError::Corrupt("owner"));
        }
        i += n;
    }
    if i != owner.len() {
        return Err(CacheError::Corrupt("owner"));
    }
    Ok(())
}
