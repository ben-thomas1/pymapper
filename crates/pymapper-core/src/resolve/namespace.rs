//! Module namespaces: the names every file binds at module level, sorted for binary search.

use rayon::prelude::*;

use crate::extract::events::Receiver;
use crate::extract::facts::{file_flags, import_flags};
use crate::index::{FileId, Index};
use crate::intern::Sym;

/// What a module-level name is bound to (low bits of [`Entry::kind`]).
pub(super) mod kind {
    /// `def` / `class`: `r` is the file-relative def row.
    pub const DEF: u8 = 0;
    /// An import: `r` is the store-absolute import row.
    pub const IMPORT: u8 = 1;
    /// An assignment target: `r` is the store-absolute binding row.
    pub const VAR: u8 = 2;
    /// Listed in `__all__` but not bound by any statement of the file (bound by a star
    /// import, a submodule import elsewhere, or dynamically).
    pub const UNBOUND: u8 = 3;
    /// `from m import *` (name [`super::STAR_NAME`], sorted last): `r` is the import row.
    pub const STAR: u8 = 4;
    /// The file calls `globals()` or `exec()`: names may be bound dynamically (name
    /// [`super::STAR_NAME`], sorted after the star imports, at most one per file).
    pub const DYNAMIC: u8 = 5;
    pub const MASK: u8 = 0b111;
    /// Flag: the name is listed in `__all__`.
    pub const IN_ALL: u8 = 1 << 7;
}

/// Name of star-import entries: sorts after every real name.
const STAR_NAME: Sym = Sym::from_index(Sym::MAX_INDEX);

/// One module-level name of a file (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub name: Sym,
    pub r: u32,
    pub kind: u8,
}

impl Entry {
    #[inline]
    pub fn base(self) -> u8 {
        self.kind & kind::MASK
    }

    #[inline]
    pub fn in_all(self) -> bool {
        self.kind & kind::IN_ALL != 0
    }
}

/// Files per parallel task.
pub(super) fn chunk_size(files: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    (files / (threads * 16)).clamp(1, 256)
}

/// Runs `per(file, out)` for every file in parallel (chunks of consecutive files) and
/// concatenates the outputs in file order. Returns the rows and the per-file starts
/// (`files + 1` entries).
pub(super) fn par_concat<T: Send>(files: usize, per: impl Fn(FileId, &mut Vec<T>) + Sync) -> (Vec<T>, Vec<u32>) {
    par_concat_with(files, |_: &mut (), f, out| per(f, out))
}

/// [`par_concat`] with per-task state `S` (scratch reused across the files of a chunk).
pub(super) fn par_concat_with<T: Send, S: Default>(
    files: usize,
    per: impl Fn(&mut S, FileId, &mut Vec<T>) + Sync,
) -> (Vec<T>, Vec<u32>) {
    let chunk = chunk_size(files);
    let parts: Vec<(Vec<T>, Vec<u32>)> = (0..files.div_ceil(chunk))
        .into_par_iter()
        .map(|c| {
            let range = c * chunk..files.min((c + 1) * chunk);
            let mut state = S::default();
            let mut out = Vec::new();
            let mut lens = Vec::with_capacity(range.len());
            for f in range {
                let before = out.len();
                per(&mut state, f as FileId, &mut out);
                lens.push((out.len() - before) as u32);
            }
            (out, lens)
        })
        .collect();
    let total: usize = parts.iter().map(|p| p.0.len()).sum();
    let mut rows = Vec::with_capacity(total);
    let mut starts = Vec::with_capacity(files + 1);
    starts.push(0u32);
    for (mut v, lens) in parts {
        let mut at = *starts.last().expect("non-empty");
        for l in lens {
            at += l;
            starts.push(at);
        }
        rows.append(&mut v);
    }
    (rows, starts)
}

/// Module-level names of every file: per file a run of entries sorted by `Sym`, one per name
/// (the first of def, import, assignment wins; then source order), followed by the file's
/// module-level star imports in source order.
#[derive(Debug, Default)]
pub(super) struct Namespaces {
    entries: Vec<Entry>,
    start: Vec<u32>,
}

impl Namespaces {
    pub fn build(ix: &Index) -> Namespaces {
        let dynamic = [ix.strings().get(b"globals"), ix.strings().get(b"exec")];
        let (entries, start) = par_concat(ix.file_count() as usize, |f, out| file_entries(ix, f, dynamic, out));
        Namespaces { entries, start }
    }

    #[inline]
    fn run(&self, f: FileId) -> &[Entry] {
        &self.entries[self.start[f as usize] as usize..self.start[f as usize + 1] as usize]
    }

    /// The module-level binding of `name` in file `f`.
    #[inline]
    pub fn get(&self, f: FileId, name: Sym) -> Option<Entry> {
        let run = self.run(f);
        run.binary_search_by_key(&name.to_raw(), |e| e.name.to_raw()).ok().map(|i| run[i])
    }

    /// Module-level star-import rows of file `f`, in source order.
    #[inline]
    pub fn stars(&self, f: FileId) -> impl Iterator<Item = u32> + '_ {
        let run = self.run(f);
        let i = run.partition_point(|e| e.name != STAR_NAME);
        run[i..].iter().filter(|e| e.kind == kind::STAR).map(|e| e.r)
    }

    /// File `f` may bind names dynamically (`globals()`, `exec()`).
    #[inline]
    pub fn is_dynamic(&self, f: FileId) -> bool {
        self.run(f).last().is_some_and(|e| e.name == STAR_NAME && e.kind == kind::DYNAMIC)
    }

    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>() + self.start.capacity() * 4
    }
}

/// Appends the namespace run of file `f` to `out`.
fn file_entries(ix: &Index, f: FileId, dynamic: [Option<Sym>; 2], out: &mut Vec<Entry>) {
    let base = out.len();
    let ff = ix.file_facts(f);
    let fv = ix.file_view(f);
    let st = ix.facts();
    let calls = &st.calls;
    let is_dynamic = ff.calls.range().any(|row| {
        calls.chain_len[row] == 1 && calls.receiver(row) == Receiver::None && dynamic.contains(&Some(calls.chain[row]))
    });
    if is_dynamic {
        out.push(Entry { name: STAR_NAME, r: u32::MAX, kind: kind::DYNAMIC });
    }
    let d0 = ff.defs.start as usize;
    for row in ff.defs.range().skip(1) {
        if st.defs.parent[row] == 0
            && let Some(name) = st.defs.name[row]
        {
            out.push(Entry { name, r: (row - d0) as u32, kind: kind::DEF });
        }
    }
    for row in ff.imports.range() {
        if st.imports.scope[row] != 0 {
            continue;
        }
        let flags = st.imports.flags[row];
        if flags & import_flags::STAR != 0 {
            out.push(Entry { name: STAR_NAME, r: row as u32, kind: kind::STAR });
            continue;
        }
        let bound = match (st.imports.alias[row], st.imports.name[row]) {
            (Some(a), _) => Some(a),
            (None, Some(n)) if flags & import_flags::FROM != 0 => Some(n),
            _ if flags & import_flags::FROM == 0 => fv.import_module(row).first().copied(),
            _ => None,
        };
        if let Some(name) = bound {
            out.push(Entry { name, r: row as u32, kind: kind::IMPORT });
        }
    }
    for row in ff.bindings.range() {
        if st.bindings.scope[row] == 0 && st.bindings.target_len[row] == 1 {
            out.push(Entry { name: st.bindings.target[row], r: row as u32, kind: kind::VAR });
        }
    }
    sort_dedup(out, base);
    if ff.has(file_flags::ALL_DEFINED) {
        let named = out[base..].partition_point(|e| e.name != STAR_NAME) + base;
        let end = out.len();
        for &name in fv.exports() {
            match out[base..named].binary_search_by_key(&name.to_raw(), |e| e.name.to_raw()) {
                Ok(i) => out[base + i].kind |= kind::IN_ALL,
                Err(_) => out.push(Entry { name, r: 0, kind: kind::UNBOUND | kind::IN_ALL }),
            }
        }
        if out.len() > end {
            sort_dedup(out, base);
        }
    }
}

/// Sorts `out[base..]` by (name, kind, row) and keeps the first entry of every name (all
/// star entries are kept).
fn sort_dedup(out: &mut Vec<Entry>, base: usize) {
    out[base..].sort_unstable_by_key(|e| (e.name.to_raw(), e.kind & kind::MASK, e.r));
    let mut w = base;
    for i in base..out.len() {
        let e = out[i];
        if w > base && out[w - 1].name == e.name && e.name != STAR_NAME {
            continue;
        }
        out[w] = e;
        w += 1;
    }
    out.truncate(w);
}
