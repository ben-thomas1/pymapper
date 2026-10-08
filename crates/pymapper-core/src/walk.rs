//! Parallel filesystem walker and gitignore matcher (design: Loading / index).
//!
//! [`walk`] finds Python sources (`*.py`, `*.pyi`) under one or more roots:
//!
//! - Parallel: one rayon task per directory on the current rayon pool (work stealing), with
//!   per-thread output buffers that are concatenated at the end. No locks are contended.
//! - Symlinks are never followed (neither to files nor to directories). Explicit roots are
//!   resolved like `find -H`: a root that is a symlink is followed.
//! - `__pycache__` and `.git` are never entered. Hidden directories (`.name`) are skipped
//!   unless [`WalkOptions::hidden`] is set.
//! - Gitignore ([`WalkOptions::gitignore`]): `.gitignore` in every directory (deeper files take
//!   precedence), `.git/info/exclude` of the enclosing repository (`.git` directory or
//!   worktree/submodule `.git` file), and, with [`WalkOptions::parents`], the ignore files of
//!   the directories between the repository top and a root. A directory containing `.git`
//!   starts a new repository: outer ignore files stop applying below it.
//! - Configured excludes ([`WalkOptions::excludes`]) use gitignore syntax, are relative to each
//!   root and are checked before (and regardless of) gitignore files.
//! - Roots themselves are never ignored or skipped. A root that is a file is yielded as is,
//!   whatever its extension.
//! - Errors (unreadable directories, entries, ignore files, missing roots) become
//!   [`Diagnostic`]s; the walk never aborts.
//!
//! Paths are stored compactly in a [`PathList`] (one byte buffer plus end offsets), sorted
//! component-wise so FileIds derived from the order are deterministic.

mod gitignore;
mod wildmatch;

#[cfg(test)]
mod tests;

pub use gitignore::{Gitignore, Match};

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Walk configuration. `Default` is the setting for first-party code.
#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// Honour `.gitignore` files and `.git/info/exclude`. Default `true`. Turn off for
    /// third-party and stdlib roots (packages ship their own `.gitignore` files, and a venv
    /// usually sits in a directory whose `.gitignore` is `*`).
    pub gitignore: bool,
    /// Also apply ignore files from the directories between the enclosing repository top and
    /// each root (as git does when run in a subdirectory). Default `true`. Costs a
    /// `canonicalize` plus one `lstat` per ancestor directory per root.
    pub parents: bool,
    /// Descend into hidden directories (name starts with `.`). Default `false`. `.git` is
    /// never entered either way.
    pub hidden: bool,
    /// Gitignore-syntax patterns relative to each root, checked before gitignore files. A
    /// negated pattern only cancels an earlier exclude; it cannot override gitignore files.
    pub excludes: Vec<String>,
    /// Sort the output component-wise (deterministic order). Default `true`. Costs one sort
    /// and one copy of the path bytes (a few ms for 60k files).
    pub sort: bool,
}

impl Default for WalkOptions {
    fn default() -> WalkOptions {
        WalkOptions { gitignore: true, parents: true, hidden: false, excludes: Vec::new(), sort: true }
    }
}

/// What went wrong for a [`Diagnostic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A root does not exist or cannot be inspected.
    Root,
    /// A directory could not be listed.
    ReadDir,
    /// An entry of a listed directory could not be read or typed.
    DirEntry,
    /// An ignore file (or `.git` file) exists but could not be read.
    IgnoreFile,
    /// The path list exceeded 4 GiB of path bytes; further files were dropped.
    PathListFull,
}

/// A non-fatal problem found while walking.
#[derive(Debug)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub path: PathBuf,
    pub error: io::Error,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.kind {
            DiagnosticKind::Root => "cannot read root",
            DiagnosticKind::ReadDir => "cannot list directory",
            DiagnosticKind::DirEntry => "cannot read directory entry in",
            DiagnosticKind::IgnoreFile => "cannot read ignore file",
            DiagnosticKind::PathListFull => "path list full, dropped",
        };
        write!(f, "{what} {}: {}", self.path.display(), self.error)
    }
}

/// Counters for one walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Directories listed (including roots).
    pub dirs: u64,
    /// Directory entries seen.
    pub entries: u64,
    /// Directories and Python files skipped by gitignore files or configured excludes.
    pub ignored: u64,
    /// Hidden directories skipped.
    pub hidden: u64,
    /// Symlinks skipped (never followed).
    pub symlinks: u64,
    /// Ignore files loaded (including `info/exclude` and parent files).
    pub ignore_files: u64,
}

impl WalkStats {
    fn add(&mut self, o: &WalkStats) {
        self.dirs += o.dirs;
        self.entries += o.entries;
        self.ignored += o.ignored;
        self.hidden += o.hidden;
        self.symlinks += o.symlinks;
        self.ignore_files += o.ignore_files;
    }
}

/// Result of [`walk`].
#[derive(Debug, Default)]
pub struct Walk {
    pub files: PathList,
    pub diagnostics: Vec<Diagnostic>,
    pub stats: WalkStats,
}

/// Append-only list of paths in a single byte buffer (no allocation per path).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathList {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl PathList {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Total bytes of all paths.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Path `i` as raw bytes. Panics if `i >= len()`.
    #[inline]
    pub fn get_bytes(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }

    /// Path `i`. Panics if `i >= len()`.
    #[inline]
    pub fn get(&self, i: usize) -> &Path {
        Path::new(OsStr::from_bytes(self.get_bytes(i)))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Path> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// Appends the concatenation of `parts`. Fails (and appends nothing) past 4 GiB.
    fn push_parts(&mut self, parts: &[&[u8]]) -> Result<(), PathListFull> {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        let end = u32::try_from(self.bytes.len() + n).map_err(|_| PathListFull)?;
        for p in parts {
            self.bytes.extend_from_slice(p);
        }
        self.ends.push(end);
        Ok(())
    }

    /// Appends one path.
    pub fn push(&mut self, path: &[u8]) -> Result<(), PathListFull> {
        self.push_parts(&[path])
    }

    /// Sorts component-wise (`/` orders before every other byte) and drops duplicates.
    fn sort_dedup(&mut self) {
        let mut order: Vec<u32> = (0..self.ends.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| path_cmp(self.get_bytes(a as usize), self.get_bytes(b as usize)));
        let mut out = PathList { bytes: Vec::with_capacity(self.bytes.len()), ends: Vec::with_capacity(self.len()) };
        let mut prev: Option<&[u8]> = None;
        for &i in &order {
            let p = self.get_bytes(i as usize);
            if prev == Some(p) {
                continue;
            }
            // Cannot fail: the output is no larger than the input.
            let _ = out.push(p);
            prev = Some(p);
        }
        *self = out;
    }

    /// Moves all paths of `other` to the end of `self`.
    fn append(&mut self, other: PathList) -> Result<(), PathListFull> {
        let base = self.bytes.len();
        u32::try_from(base + other.bytes.len()).map_err(|_| PathListFull)?;
        self.bytes.extend_from_slice(&other.bytes);
        self.ends.extend(other.ends.iter().map(|&e| e + base as u32));
        Ok(())
    }
}

/// A [`PathList`] cannot hold more than 4 GiB of path bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathListFull;

impl std::fmt::Display for PathListFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("path list exceeds 4 GiB of path bytes")
    }
}

impl std::error::Error for PathListFull {}

/// Component-wise byte order: like comparing the byte strings with `/` mapped below 0x00, so
/// `a/b` sorts before `a-b` and `a.b`, and a directory's contents stay together.
fn path_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    let pos = a[..n].iter().zip(&b[..n]).position(|(x, y)| x != y);
    match pos {
        Some(i) => {
            let key = |c: u8| if c == b'/' { 0u16 } else { c as u16 + 1 };
            key(a[i]).cmp(&key(b[i]))
        }
        None => a.len().cmp(&b.len()),
    }
}

/// Whether a file name is a Python source or stub.
#[inline]
pub fn is_python(name: &[u8]) -> bool {
    name.ends_with(b".py") || name.ends_with(b".pyi")
}

/// One ignore file applying to a subtree. Directories without an ignore file share their
/// parent's node (an `Arc` bump), so the chain only grows where ignore files exist.
struct IgnoreNode {
    parent: Option<Arc<IgnoreNode>>,
    gi: Gitignore,
    /// Offset in the match key where this file's directory starts.
    base: usize,
}

/// Per-root constants.
///
/// Paths below a root are `out_prefix + rel`. Gitignore matching uses the *match key*
/// `key_prefix + rel`, which is the path relative to the enclosing repository top (or to the
/// root when there is none), so ancestor ignore files see the paths they expect.
struct RootCtx {
    out_len: usize,
    key_prefix: Box<[u8]>,
}

struct DirTask {
    /// Directory path ending in `/`, or empty for a root given as `.`.
    path: Box<[u8]>,
    root: u32,
    ignore: Option<Arc<IgnoreNode>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Copy)]
struct Ent {
    start: u32,
    end: u32,
    kind: Kind,
}

/// Per-thread state: reusable scratch buffers plus this thread's share of the output.
#[derive(Default)]
struct Worker {
    names: Vec<u8>,
    ents: Vec<Ent>,
    key: Vec<u8>,
    files: PathList,
    diags: Vec<Diagnostic>,
    stats: WalkStats,
}

impl Worker {
    fn diag(&mut self, kind: DiagnosticKind, path: &Path, error: io::Error) {
        self.diags.push(Diagnostic { kind, path: path.to_path_buf(), error });
    }
}

struct Shared<'a> {
    opts: &'a WalkOptions,
    roots: Vec<RootCtx>,
    excludes: Option<Gitignore>,
    /// Indexed by rayon thread index; each slot is only ever locked by its own thread.
    workers: Vec<Mutex<Worker>>,
}

#[inline]
fn bytes_path(b: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(b))
}

/// Walks `roots` in parallel on the current rayon pool and returns the Python files found.
pub fn walk<P: AsRef<Path>>(roots: &[P], opts: &WalkOptions) -> Walk {
    let threads = rayon::current_num_threads().max(1);
    let excludes = if opts.excludes.is_empty() { None } else { Some(Gitignore::from_patterns(&opts.excludes)) };
    let mut main = Worker::default();
    let mut ctxs = Vec::with_capacity(roots.len());
    let mut seeds = Vec::with_capacity(roots.len());
    for root in roots {
        let root = root.as_ref();
        let meta = match fs::metadata(root) {
            Ok(m) => m,
            Err(e) => {
                main.diag(DiagnosticKind::Root, root, e);
                continue;
            }
        };
        if !meta.is_dir() {
            if meta.is_file() && main.files.push(root.as_os_str().as_bytes()).is_err() {
                main.diag(DiagnosticKind::PathListFull, root, io::Error::other(PathListFull));
            }
            continue;
        }
        let out = root_prefix(root.as_os_str().as_bytes());
        let (key_prefix, ignore) = if opts.gitignore && opts.parents {
            parent_chain(root, &mut main)
        } else {
            (Box::default(), None)
        };
        ctxs.push(RootCtx { out_len: out.len(), key_prefix });
        seeds.push(DirTask { path: out, root: (ctxs.len() - 1) as u32, ignore });
    }
    let sh = Shared { opts, roots: ctxs, excludes, workers: (0..threads).map(|_| Mutex::default()).collect() };
    rayon::scope(|s| {
        for task in seeds {
            let sh = &sh;
            s.spawn(move |s| run_dir(s, sh, task));
        }
    });
    let mut out = Walk { files: main.files, diagnostics: main.diags, stats: main.stats };
    for slot in sh.workers {
        let w = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
        out.stats.add(&w.stats);
        out.diagnostics.extend(w.diags);
        if out.files.append(w.files).is_err() {
            out.diagnostics.push(Diagnostic {
                kind: DiagnosticKind::PathListFull,
                path: PathBuf::new(),
                error: io::Error::other(PathListFull),
            });
        }
    }
    if opts.sort {
        out.files.sort_dedup();
        out.diagnostics.sort_by(|a, b| {
            path_cmp(a.path.as_os_str().as_bytes(), b.path.as_os_str().as_bytes())
                .then((a.kind as u8).cmp(&(b.kind as u8)))
        });
    }
    out
}

/// Output prefix for paths below `root`: `root/`, or empty for `.`.
fn root_prefix(root: &[u8]) -> Box<[u8]> {
    let mut r = root;
    while r.len() > 1 && r.ends_with(b"/") {
        r = &r[..r.len() - 1];
    }
    if r == b"." {
        return Box::default();
    }
    let mut v = Vec::with_capacity(r.len() + 1);
    v.extend_from_slice(r);
    if !r.ends_with(b"/") {
        v.push(b'/');
    }
    v.into_boxed_slice()
}

/// Loads the ignore file at `path` (if it exists) as a node on top of `parent`.
fn load_ignore(
    path: &Path,
    base: usize,
    parent: Option<Arc<IgnoreNode>>,
    w: &mut Worker,
) -> Option<Arc<IgnoreNode>> {
    match fs::read(path) {
        Ok(src) => {
            w.stats.ignore_files += 1;
            let gi = Gitignore::parse(&src);
            if gi.is_empty() { parent } else { Some(Arc::new(IgnoreNode { parent, gi, base })) }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => parent,
        Err(e) => {
            w.diag(DiagnosticKind::IgnoreFile, path, e);
            parent
        }
    }
}

/// Path of `info/exclude` for the repository whose top is `top`. `dot_git_is_dir` tells
/// whether `top/.git` is a directory (normal repo) or a file (`gitdir: ...` pointer of a
/// worktree or submodule; the exclude file then lives in the common dir).
fn exclude_path(top: &Path, dot_git_is_dir: bool, w: &mut Worker) -> Option<PathBuf> {
    let dot_git = top.join(".git");
    if dot_git_is_dir {
        return Some(dot_git.join("info/exclude"));
    }
    let src = match fs::read(&dot_git) {
        Ok(s) => s,
        Err(e) => {
            w.diag(DiagnosticKind::IgnoreFile, &dot_git, e);
            return None;
        }
    };
    let target = src.strip_prefix(b"gitdir:")?.trim_ascii();
    // `join` keeps an absolute target as is and resolves a relative one against `top`.
    let git_dir = top.join(bytes_path(target));
    let common = match fs::read(git_dir.join("commondir")) {
        Ok(c) => git_dir.join(bytes_path(c.trim_ascii())),
        Err(_) => git_dir,
    };
    Some(common.join("info/exclude"))
}

/// For a root inside a repository: the root's path relative to the repository top (plus `/`)
/// and the chain of ignore files from the top down to the root's parent. The root's own
/// ignore file is loaded by the walk itself.
fn parent_chain(root: &Path, w: &mut Worker) -> (Box<[u8]>, Option<Arc<IgnoreNode>>) {
    let none = (Box::default(), None);
    // A root that is itself a repository top: outer ignore files do not apply, and the walk
    // loads its `info/exclude` and `.gitignore` when it lists the root.
    if fs::symlink_metadata(root.join(".git")).is_ok_and(|m| m.is_dir() || m.is_file()) {
        return none;
    }
    let Ok(canon) = fs::canonicalize(root) else { return none };
    let mut found = None;
    for anc in canon.ancestors().skip(1) {
        if let Ok(m) = fs::symlink_metadata(anc.join(".git"))
            && (m.is_dir() || m.is_file())
        {
            found = Some((anc, m.is_dir()));
            break;
        }
    }
    let Some((top, dot_git_is_dir)) = found else { return none };
    let Ok(rel) = canon.strip_prefix(top) else { return none };
    let rel = rel.as_os_str().as_bytes();
    let mut chain = None;
    if let Some(p) = exclude_path(top, dot_git_is_dir, w) {
        chain = load_ignore(&p, 0, None, w);
    }
    // Ignore files of `top` and of every directory strictly between `top` and the root.
    let mut dir = top.to_path_buf();
    let mut base = 0usize;
    for comp in rel.split(|&c| c == b'/') {
        let file = dir.join(".gitignore");
        if fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
            chain = load_ignore(&file, base, chain, w);
        }
        dir.push(bytes_path(comp));
        base += comp.len() + 1;
    }
    let mut key_prefix = Vec::with_capacity(rel.len() + 1);
    key_prefix.extend_from_slice(rel);
    key_prefix.push(b'/');
    (key_prefix.into_boxed_slice(), chain)
}

/// Whether the entry `name` (directory part of the key already in `key[..dir_len]`) is
/// ignored by the configured excludes or the gitignore chain.
fn is_ignored(
    key: &mut Vec<u8>,
    dir_len: usize,
    name: &[u8],
    is_dir: bool,
    excludes: Option<(&Gitignore, usize)>,
    chain: Option<&Arc<IgnoreNode>>,
) -> bool {
    key.truncate(dir_len);
    key.extend_from_slice(name);
    if let Some((ex, base)) = excludes
        && ex.matched(&key[base..], is_dir) == Match::Ignore
    {
        return true;
    }
    let mut node = chain;
    while let Some(n) = node {
        match n.gi.matched(&key[n.base..], is_dir) {
            Match::Ignore => return true,
            Match::Whitelist => return false,
            Match::None => node = n.parent.as_ref(),
        }
    }
    false
}

/// Lists one directory, records its Python files and spawns a task per subdirectory.
fn run_dir<'s>(s: &rayon::Scope<'s>, sh: &'s Shared<'s>, task: DirTask) {
    let slot = rayon::current_thread_index().unwrap_or(0) % sh.workers.len();
    // Only this thread locks this slot, and nothing below re-enters the scheduler while the
    // guard is held (`spawn` only queues), so the lock is never contended.
    let mut guard = sh.workers[slot].lock().unwrap_or_else(PoisonError::into_inner);
    let w = &mut *guard;
    let opts = sh.opts;
    let dir = if task.path.is_empty() { Path::new(".") } else { bytes_path(&task.path) };
    w.stats.dirs += 1;
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            w.diag(DiagnosticKind::ReadDir, dir, e);
            return;
        }
    };

    // Pass 1: buffer the names, so this directory's own ignore files are known before any
    // entry is classified.
    w.names.clear();
    w.ents.clear();
    let mut has_gitignore = false;
    let mut dot_git: Option<Kind> = None;
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                w.diag(DiagnosticKind::DirEntry, dir, e);
                continue;
            }
        };
        // `DirEntry::file_type` uses the readdir type when available and never follows links.
        let kind = match entry.file_type() {
            Ok(t) if t.is_dir() => Kind::Dir,
            Ok(t) if t.is_file() => Kind::File,
            Ok(t) if t.is_symlink() => Kind::Symlink,
            Ok(_) => Kind::Other,
            Err(e) => {
                w.diag(DiagnosticKind::DirEntry, &entry.path(), e);
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.as_bytes();
        if opts.gitignore {
            if name == b".gitignore" && kind == Kind::File {
                has_gitignore = true;
            } else if name == b".git" && (kind == Kind::Dir || kind == Kind::File) {
                dot_git = Some(kind);
            }
        }
        let (Ok(start), Ok(end)) = (u32::try_from(w.names.len()), u32::try_from(w.names.len() + name.len()))
        else {
            // Over 4 GiB of names in one directory: cannot happen on real filesystems.
            break;
        };
        w.names.extend_from_slice(name);
        w.ents.push(Ent { start, end, kind });
    }
    w.stats.entries += w.ents.len() as u64;

    // This directory's ignore chain.
    let ctx = &sh.roots[task.root as usize];
    let dir_key_len = ctx.key_prefix.len() + task.path.len() - ctx.out_len;
    let mut chain = task.ignore;
    if let Some(kind) = dot_git {
        // A repository top: outer ignore files stop applying; `info/exclude` is the base.
        chain = None;
        if let Some(p) = exclude_path(dir, kind == Kind::Dir, w) {
            chain = load_ignore(&p, dir_key_len, None, w);
        }
    }
    if has_gitignore {
        chain = load_ignore(&dir.join(".gitignore"), dir_key_len, chain, w);
    }
    let excludes = sh.excludes.as_ref().map(|g| (g, ctx.key_prefix.len()));
    let check = chain.is_some() || excludes.is_some();
    if check {
        w.key.clear();
        w.key.extend_from_slice(&ctx.key_prefix);
        w.key.extend_from_slice(&task.path[ctx.out_len..]);
    }

    // Pass 2: classify.
    for &Ent { start, end, kind } in &w.ents {
        let name = &w.names[start as usize..end as usize];
        match kind {
            Kind::File => {
                if !is_python(name) {
                    continue;
                }
                if check && is_ignored(&mut w.key, dir_key_len, name, false, excludes, chain.as_ref()) {
                    w.stats.ignored += 1;
                    continue;
                }
                if w.files.push_parts(&[&task.path, name]).is_err() {
                    let path = bytes_path(&task.path).join(bytes_path(name));
                    w.diags.push(Diagnostic {
                        kind: DiagnosticKind::PathListFull,
                        path,
                        error: io::Error::other(PathListFull),
                    });
                }
            }
            Kind::Dir => {
                if name == b"__pycache__" || name == b".git" {
                    continue;
                }
                if name[0] == b'.' && !opts.hidden {
                    w.stats.hidden += 1;
                    continue;
                }
                if check && is_ignored(&mut w.key, dir_key_len, name, true, excludes, chain.as_ref()) {
                    w.stats.ignored += 1;
                    continue;
                }
                let mut path = Vec::with_capacity(task.path.len() + name.len() + 1);
                path.extend_from_slice(&task.path);
                path.extend_from_slice(name);
                path.push(b'/');
                let sub = DirTask { path: path.into_boxed_slice(), root: task.root, ignore: chain.clone() };
                s.spawn(move |s| run_dir(s, sh, sub));
            }
            Kind::Symlink => w.stats.symlinks += 1,
            Kind::Other => {}
        }
    }
}
