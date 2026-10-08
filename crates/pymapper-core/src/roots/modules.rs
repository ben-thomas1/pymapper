//! Module names and import-system semantics over a set of files (PEP 420, PEP 561).
//!
//! [`ModuleMapBuilder`] takes files as (import root, path relative to the root) and
//! [`ModuleMapBuilder::finish`] decides, the way CPython's `PathFinder` would, which file each
//! dotted name resolves to:
//!
//! - Roots are searched in order (the [`super::RootId`] order is `sys.path` order).
//! - Within one root directory a regular package (`name/__init__.py[i]`) beats a module
//!   (`name.py[i]`), which beats a namespace portion (a directory without `__init__`).
//! - The first root with a package or module wins outright; only when no root has one does
//!   the name become a namespace package whose portions are all the roots having the directory.
//! - Children are searched only in the parent's `__path__`: the winning root of a regular
//!   package, every portion of a namespace package, nothing below a plain module.
//! - `.pyi` is preferred over `.py` for the same module in the same root (the `.py` becomes
//!   [`FileStatus::StubShadowed`]; it is still the implementation). A PEP 561 stub-only package
//!   `foo-stubs/` provides `.pyi` files for `foo`.
//!
//! Cost: O(F·d) to intern names (F files, d package depth) plus a sort of the
//! (name, root) entries, O(E log E). No filesystem access.

use super::RootId;
use crate::intern::{Interner, Sym};

const NONE: u32 = u32::MAX;

const DIR: u8 = 1;
const INIT_PY: u8 = 2;
const INIT_PYI: u8 = 4;
const MOD_PY: u8 = 8;
const MOD_PYI: u8 = 16;
const INIT: u8 = INIT_PY | INIT_PYI;
const MODULE: u8 = MOD_PY | MOD_PYI;

/// What a file is, after resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FileStatus {
    /// The file an import of its module name resolves to.
    Active,
    /// A `.py` whose module has a `.pyi` in the same root: the stub is preferred for symbol
    /// shapes; this file is still the implementation.
    StubShadowed,
    /// Unreachable by import: an earlier root, a regular package or a module wins, the parent
    /// package does not search this root, or another file of the same kind already claimed the
    /// name.
    Shadowed,
    /// The path has a component that is not an identifier (`my-script.py`, `a.b/c.py`), or is a
    /// root's own `__init__.py`.
    NotImportable,
    /// Not under any import root.
    Outside,
}

/// How a dotted name resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// `name.py` / `name.pyi`.
    Module,
    /// `name/__init__.py[i]`.
    Package,
    /// PEP 420 namespace package: directories without `__init__` in one or more roots.
    Namespace,
}

#[derive(Clone, Copy)]
struct Entry {
    name: u32,
    root: u32,
    bits: u8,
}

#[derive(Clone, Copy)]
struct FileEntry {
    name: u32,
    root: u32,
    bit: u8,
    status: FileStatus,
}

/// Collects files; see the module docs.
#[derive(Default)]
pub struct ModuleMapBuilder {
    names: Interner,
    parent: Vec<u32>,
    depth: Vec<u16>,
    /// Last root that recorded a `DIR` entry per name (cheap duplicate suppression).
    last_dir_root: Vec<u32>,
    entries: Vec<Entry>,
    files: Vec<FileEntry>,
    name_buf: Vec<u8>,
    comps: Vec<(usize, usize)>,
}

/// Whether `s` is a valid module name component: ASCII letters, digits and `_` (not leading
/// digit), or any non-ASCII byte (no UTF-8 / NFKC validation, like the lexer).
pub fn is_identifier(s: &[u8]) -> bool {
    match s.first() {
        None => false,
        Some(c) if c.is_ascii_digit() => false,
        _ => s.iter().all(|&b| b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80),
    }
}

impl ModuleMapBuilder {
    pub fn new() -> ModuleMapBuilder {
        ModuleMapBuilder::default()
    }

    /// Number of files added.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Adds a file outside every import root. Returns its file index.
    pub fn add_outside(&mut self) -> u32 {
        self.push_file(NONE, NONE, 0, FileStatus::Outside)
    }

    /// Adds the file `rel` (relative to `root`, `/`-separated, ending in `.py` or `.pyi`).
    /// `prefix` is the root's dotted module prefix (editable mappings; empty otherwise). With
    /// `single`, the root is the file itself and `rel` is its file name: the module is `prefix`.
    /// Returns the file index (files are numbered in the order added).
    pub fn add(&mut self, root: RootId, prefix: &[u8], rel: &[u8], single: bool) -> u32 {
        let (stem, bit) = if let Some(s) = rel.strip_suffix(b".pyi") {
            (s, MOD_PYI)
        } else if let Some(s) = rel.strip_suffix(b".py") {
            (s, MOD_PY)
        } else {
            return self.push_file(NONE, root, 0, FileStatus::NotImportable);
        };
        self.comps.clear();
        self.name_buf.clear();
        let mut ok = true;
        let mut push = |b: &mut Vec<u8>, comps: &mut Vec<(usize, usize)>, c: &[u8]| {
            ok &= is_identifier(c);
            if !b.is_empty() {
                b.push(b'.');
            }
            comps.push((b.len(), c.len()));
            b.extend_from_slice(c);
        };
        if !prefix.is_empty() {
            for c in prefix.split(|&b| b == b'.') {
                push(&mut self.name_buf, &mut self.comps, c);
            }
        }
        let mut bit = bit;
        if !single {
            let mut parts = stem.split(|&b| b == b'/').peekable();
            let mut first = prefix.is_empty();
            while let Some(c) = parts.next() {
                let last = parts.peek().is_none();
                if last {
                    if c == b"__init__" {
                        bit = if bit == MOD_PYI { INIT_PYI } else { INIT_PY };
                        break;
                    }
                } else if first {
                    // PEP 561 stub-only package.
                    if let Some(base) = c.strip_suffix(b"-stubs") {
                        push(&mut self.name_buf, &mut self.comps, base);
                        first = false;
                        continue;
                    }
                }
                first = false;
                push(&mut self.name_buf, &mut self.comps, c);
            }
        }
        if !ok || self.comps.is_empty() {
            return self.push_file(NONE, root, 0, FileStatus::NotImportable);
        }
        // Intern every prefix; all but the full name are directories in this root.
        let mut parent = NONE;
        let n = self.comps.len();
        for k in 0..n {
            let (start, len) = self.comps[k];
            let end = start + len;
            let sym = self.names.intern(&self.name_buf[..end]);
            let id = sym.index() as u32;
            if id as usize == self.parent.len() {
                self.parent.push(parent);
                self.depth.push(k as u16);
                self.last_dir_root.push(NONE);
            }
            let is_full = k + 1 == n;
            let bits = if !is_full {
                DIR
            } else if bit & INIT != 0 {
                DIR | bit
            } else {
                bit
            };
            if bits == DIR {
                if self.last_dir_root[id as usize] != root {
                    self.last_dir_root[id as usize] = root;
                    self.entries.push(Entry { name: id, root, bits });
                }
            } else {
                self.entries.push(Entry { name: id, root, bits });
            }
            parent = id;
        }
        self.push_file(parent, root, bit, FileStatus::Shadowed)
    }

    fn push_file(&mut self, name: u32, root: u32, bit: u8, status: FileStatus) -> u32 {
        let i = self.files.len() as u32;
        self.files.push(FileEntry { name, root, bit, status });
        i
    }

    /// Resolves every name and file.
    pub fn finish(self) -> ModuleMap {
        let ModuleMapBuilder { names, parent, depth, mut entries, mut files, .. } = self;
        let n = names.len();

        // (name, root) entries, merged, as CSR by name.
        entries.sort_unstable_by_key(|e| (e.name, e.root));
        let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
        for e in entries {
            match merged.last_mut() {
                Some(m) if m.name == e.name && m.root == e.root => m.bits |= e.bits,
                _ => merged.push(e),
            }
        }
        let mut start = vec![0u32; n + 1];
        for e in &merged {
            start[e.name as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }

        // Names by depth (parents first), counting sort.
        let max_depth = depth.iter().copied().max().map_or(0, usize::from);
        let mut by_depth = vec![0u32; max_depth + 2];
        for &d in &depth {
            by_depth[usize::from(d) + 1] += 1;
        }
        for i in 0..=max_depth {
            by_depth[i + 1] += by_depth[i];
        }
        let mut order = vec![0u32; n];
        for (i, &d) in depth.iter().enumerate() {
            let slot = &mut by_depth[usize::from(d)];
            order[*slot as usize] = i as u32;
            *slot += 1;
        }

        let mut kind = vec![KIND_UNRESOLVED; n];
        let mut win_root = vec![NONE; n];
        let mut win_bits = vec![0u8; n];
        // `__path__` roots of packages: a range into `path_pool`.
        let mut path = vec![(0u32, 0u32); n];
        let mut path_pool: Vec<u32> = Vec::new();
        for &id in &order {
            let i = id as usize;
            let p = parent[i];
            let (allowed, all) = if p == NONE {
                ((0, 0), true)
            } else if matches!(kind[p as usize], KIND_PACKAGE | KIND_NAMESPACE) {
                (path[p as usize], false)
            } else {
                continue;
            };
            let cands = &merged[start[i] as usize..start[i + 1] as usize];
            // Indices, not a slice: `path_pool` grows below.
            let (a_start, a_end) = (allowed.0 as usize, (allowed.0 + allowed.1) as usize);
            let mut first_dir = path_pool.len() as u32;
            let mut winner: Option<Entry> = None;
            // Both lists are sorted by root: merge.
            let mut a = a_start;
            for e in cands {
                if !all {
                    while a < a_end && path_pool[a] < e.root {
                        a += 1;
                    }
                    if a == a_end {
                        break;
                    }
                    if path_pool[a] != e.root {
                        continue;
                    }
                }
                if e.bits & (INIT | MODULE) != 0 {
                    winner = Some(*e);
                    break;
                }
                path_pool.push(e.root);
            }
            if let Some(w) = winner {
                path_pool.truncate(first_dir as usize);
                win_root[i] = w.root;
                win_bits[i] = w.bits;
                if w.bits & INIT != 0 {
                    kind[i] = KIND_PACKAGE;
                    first_dir = path_pool.len() as u32;
                    path_pool.push(w.root);
                    path[i] = (first_dir, 1);
                } else {
                    kind[i] = KIND_MODULE;
                }
            } else if path_pool.len() as u32 > first_dir {
                kind[i] = KIND_NAMESPACE;
                path[i] = (first_dir, path_pool.len() as u32 - first_dir);
            }
        }

        // Files.
        let mut active = vec![NONE; n];
        let mut implementation = vec![NONE; n];
        for (f, file) in files.iter_mut().enumerate() {
            if file.status != FileStatus::Shadowed {
                continue;
            }
            let i = file.name as usize;
            let k = kind[i];
            let matches = win_root[i] == file.root
                && ((k == KIND_PACKAGE && file.bit & INIT != 0) || (k == KIND_MODULE && file.bit & MODULE != 0));
            if !matches {
                continue;
            }
            let has_stub = win_bits[i] & if k == KIND_PACKAGE { INIT_PYI } else { MOD_PYI } != 0;
            let is_stub = file.bit & (INIT_PYI | MOD_PYI) != 0;
            if has_stub && !is_stub {
                if implementation[i] == NONE {
                    implementation[i] = f as u32;
                    file.status = FileStatus::StubShadowed;
                }
            } else if active[i] == NONE {
                active[i] = f as u32;
                file.status = FileStatus::Active;
            }
        }
        for i in 0..n {
            if implementation[i] == NONE && active[i] != NONE && files[active[i] as usize].bit & (MOD_PY | INIT_PY) != 0 {
                implementation[i] = active[i];
            }
        }

        let file_name = files.iter().map(|f| f.name).collect();
        let file_root = files.iter().map(|f| f.root).collect();
        let file_status = files.iter().map(|f| f.status).collect();
        ModuleMap { names, parent, kind, win_root, path, path_pool, active, implementation, file_name, file_root, file_status }
    }
}

const KIND_UNRESOLVED: u8 = 0;
const KIND_MODULE: u8 = 1;
const KIND_PACKAGE: u8 = 2;
const KIND_NAMESPACE: u8 = 3;

/// Result of [`ModuleMapBuilder::finish`]: dotted names (interned), how each resolves, and the
/// status of every file. File indices are the ones returned by the builder.
pub struct ModuleMap {
    names: Interner,
    parent: Vec<u32>,
    kind: Vec<u8>,
    win_root: Vec<u32>,
    path: Vec<(u32, u32)>,
    path_pool: Vec<u32>,
    active: Vec<u32>,
    implementation: Vec<u32>,
    file_name: Vec<u32>,
    file_root: Vec<u32>,
    file_status: Vec<FileStatus>,
}

impl ModuleMap {
    /// Every dotted name seen (modules, packages, and the directories leading to them).
    pub fn names(&self) -> &Interner {
        &self.names
    }

    /// Number of files.
    pub fn file_count(&self) -> usize {
        self.file_status.len()
    }

    /// The module name of a file (`None` if not importable / outside every root). A shadowed
    /// file still has the name it would have.
    pub fn file_module(&self, file: u32) -> Option<Sym> {
        let n = self.file_name[file as usize];
        (n != NONE).then(|| Sym::from_index(n))
    }

    /// Dotted module name of a file as bytes.
    pub fn file_module_name(&self, file: u32) -> Option<&[u8]> {
        self.file_module(file).map(|s| self.names.resolve(s))
    }

    pub fn file_status(&self, file: u32) -> FileStatus {
        self.file_status[file as usize]
    }

    /// The import root a file was added under.
    pub fn file_root(&self, file: u32) -> Option<RootId> {
        let r = self.file_root[file as usize];
        (r != NONE).then_some(r)
    }

    /// Interned id of a dotted name.
    pub fn lookup(&self, dotted: &[u8]) -> Option<Sym> {
        self.names.get(dotted)
    }

    /// Parent package of a name (`a.b` for `a.b.c`).
    pub fn parent(&self, name: Sym) -> Option<Sym> {
        let p = self.parent[name.index()];
        (p != NONE).then(|| Sym::from_index(p))
    }

    /// How `name` resolves; `None` if it is not importable (shadowed by a module, or its
    /// parent does not search any root holding it).
    pub fn kind(&self, name: Sym) -> Option<ModuleKind> {
        match self.kind[name.index()] {
            KIND_MODULE => Some(ModuleKind::Module),
            KIND_PACKAGE => Some(ModuleKind::Package),
            KIND_NAMESPACE => Some(ModuleKind::Namespace),
            _ => None,
        }
    }

    /// The file `name` resolves to (the stub if there is one); `None` for namespace packages.
    pub fn file(&self, name: Sym) -> Option<u32> {
        let f = self.active[name.index()];
        (f != NONE).then_some(f)
    }

    /// The `.py` implementing `name` (the active file itself when it is a `.py`).
    pub fn implementation(&self, name: Sym) -> Option<u32> {
        let f = self.implementation[name.index()];
        (f != NONE).then_some(f)
    }

    /// The root `name` was found in (`None` for namespace packages and unresolved names).
    pub fn root(&self, name: Sym) -> Option<RootId> {
        let r = self.win_root[name.index()];
        (r != NONE).then_some(r)
    }

    /// The package's `__path__` roots: the winning root of a regular package, every portion of
    /// a namespace package, empty otherwise.
    pub fn search_roots(&self, name: Sym) -> &[RootId] {
        let (s, l) = self.path[name.index()];
        &self.path_pool[s as usize..(s + l) as usize]
    }

    /// Approximate heap bytes.
    pub fn heap_bytes(&self) -> usize {
        self.names.heap_bytes()
            + (self.parent.capacity() + self.win_root.capacity() + self.active.capacity() + self.implementation.capacity()) * 4
            + self.kind.capacity()
            + self.path.capacity() * 8
            + self.path_pool.capacity() * 4
            + (self.file_name.capacity() + self.file_root.capacity()) * 4
            + self.file_status.capacity()
    }
}
