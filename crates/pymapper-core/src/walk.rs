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
//! - Gitignore ([`WalkOptions::gitignore`]): `.gitignore` and `.ignore` in every directory
//!   (deeper files take precedence; `.ignore` beats `.gitignore` of the same directory),
//!   `.git/info/exclude` of the enclosing repository (`.git` directory or worktree/submodule
//!   `.git` file), and, with [`WalkOptions::parents`], the ignore files of the directories
//!   between the repository top and a root. A directory containing `.git` starts a new
//!   repository: outer ignore files stop applying below it. Ignore files also apply outside
//!   any repository. The global `core.excludesFile` is not read.
//! - Configured excludes ([`WalkOptions::excludes`]) use gitignore syntax, are relative to
//!   [`WalkOptions::exclude_base`] (or each root) and are checked before (and regardless of)
//!   ignore files.
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

pub use gitignore::{Gitignore, Match, PatternError};

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::Config;

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
    /// Gitignore-syntax patterns, checked before gitignore files. A negated pattern only
    /// cancels an earlier exclude; it cannot override gitignore files. Invalid patterns
    /// ([`Gitignore::check_pattern`]) are reported as [`DiagnosticKind::Exclude`].
    pub excludes: Vec<String>,
    /// Directory the `excludes` are relative to (the project root). `None`: relative to each
    /// root. A root outside this directory also matches them relative to itself.
    pub exclude_base: Option<PathBuf>,
    /// Sort the output component-wise (deterministic order). Default `true`. Costs one sort
    /// and one copy of the path bytes (a few ms for 60k files).
    pub sort: bool,
    /// Also list the `pyproject.toml` files found below directory roots in
    /// [`Walk::projects`] (project units, see [`crate::roots::ProjectUnits`]). Default `false`;
    /// on for first-party code. Ignore files and excludes apply to them as to sources.
    pub projects: bool,
    /// File-name globs (wildmatch syntax, e.g. `*.yaml`) of non-Python config files to list in
    /// [`Walk::configs`] (config-file evidence for dead code). Empty (default): none. Ignore
    /// files and excludes apply; lockfiles ([`is_lockfile`]) are never listed.
    pub configs: Vec<String>,
}

impl Default for WalkOptions {
    fn default() -> WalkOptions {
        WalkOptions {
            gitignore: true,
            parents: true,
            hidden: false,
            excludes: Vec::new(),
            exclude_base: None,
            sort: true,
            projects: false,
            configs: Vec::new(),
        }
    }
}

impl WalkOptions {
    /// Options for the first-party `paths` of a loaded configuration: `respect-gitignore`,
    /// `exclude` + `extend-exclude` relative to the project root.
    pub fn first_party(config: &Config, project_root: &Path) -> WalkOptions {
        WalkOptions {
            gitignore: config.respect_gitignore,
            excludes: config.all_excludes().map(str::to_owned).collect(),
            exclude_base: Some(project_root.to_path_buf()),
            projects: true,
            configs: if config.analysis.config_evidence { config.analysis.config_evidence_globs.clone() } else { Vec::new() },
            ..WalkOptions::default()
        }
    }

    /// Options for third-party and stdlib roots: every Python file except `__pycache__` and
    /// `.git` contents, hidden directories included, no ignore files, no excludes.
    pub fn everything() -> WalkOptions {
        WalkOptions { gitignore: false, parents: false, hidden: true, ..WalkOptions::default() }
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
    /// A configured exclude pattern is invalid (the path is the pattern). It is skipped.
    Exclude,
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
            DiagnosticKind::Exclude => "invalid exclude pattern",
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
    /// `pyproject.toml` files, sorted component-wise, without duplicates (only with
    /// [`WalkOptions::projects`]).
    pub projects: PathList,
    /// Config files matching [`WalkOptions::configs`], sorted component-wise, without
    /// duplicates.
    pub configs: PathList,
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

}

/// Files found by one worker: each path relative to its root (the root's output prefix is
/// left out, so a deep absolute root is not repeated per file while walking) plus the root
/// index of each file.
#[derive(Default)]
struct Found {
    rel: PathList,
    root: Vec<u32>,
}

impl Found {
    fn push(&mut self, root: u32, parts: &[&[u8]]) -> Result<(), PathListFull> {
        self.rel.push_parts(parts)?;
        self.root.push(root);
        Ok(())
    }
}

/// Joins the per-worker results into one exactly sized list of full paths (`prefixes[root]`
/// followed by the relative path); with `sort`, ordered component-wise (`/` before every
/// other byte) without duplicates. Worker results that would push the total past 4 GiB are
/// left out; the second value counts them.
fn merge(found: Vec<Found>, prefixes: &[&[u8]], sort: bool) -> (PathList, usize) {
    let mut bytes = 0usize;
    let mut n = 0usize;
    let mut dropped = 0usize;
    let mut kept = Vec::with_capacity(found.len());
    for f in found {
        let mut fb = f.rel.bytes.len();
        for &r in &f.root {
            fb += prefixes[r as usize].len();
        }
        if u32::try_from(bytes + fb).is_err() {
            dropped += 1;
            continue;
        }
        bytes += fb;
        n += f.root.len();
        kept.push(f);
    }
    let mut out = PathList { bytes: Vec::with_capacity(bytes), ends: Vec::with_capacity(n) };
    let part = |c: u32, i: u32| {
        let f = &kept[c as usize];
        (f.root[i as usize], f.rel.get_bytes(i as usize))
    };
    let cmp = |a: (u32, &[u8]), b: (u32, &[u8])| {
        if a.0 == b.0 {
            path_cmp(a.1, b.1)
        } else {
            joined_cmp(prefixes[a.0 as usize], a.1, prefixes[b.0 as usize], b.1)
        }
    };
    if sort {
        // Sort (worker, index) pairs and copy once into the exact-size output.
        let mut order: Vec<(u32, u32)> = Vec::with_capacity(n);
        for (c, f) in kept.iter().enumerate() {
            for i in 0..f.root.len() {
                order.push((c as u32, i as u32));
            }
        }
        order.sort_unstable_by(|&(c, i), &(d, j)| cmp(part(c, i), part(d, j)));
        let mut prev: Option<(u32, &[u8])> = None;
        for &(c, i) in &order {
            let cur = part(c, i);
            if prev.is_some_and(|p| cmp(p, cur) == Ordering::Equal) {
                continue;
            }
            // Cannot fail: the total was checked above.
            let _ = out.push_parts(&[prefixes[cur.0 as usize], cur.1]);
            prev = Some(cur);
        }
    } else {
        for (c, f) in kept.iter().enumerate() {
            for i in 0..f.root.len() {
                let (r, rel) = part(c as u32, i as u32);
                let _ = out.push_parts(&[prefixes[r as usize], rel]);
            }
        }
    }
    (out, dropped)
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
    match first_diff(a, b) {
        Some(i) => byte_key(a[i]).cmp(&byte_key(b[i])),
        None => a.len().cmp(&b.len()),
    }
}

#[inline]
fn byte_key(c: u8) -> u16 {
    if c == b'/' { 0 } else { c as u16 + 1 }
}

/// First index below the shorter length where `a` and `b` differ.
#[inline]
fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    a[..n].iter().zip(&b[..n]).position(|(x, y)| x != y)
}

/// [`path_cmp`] of the concatenations `a0 a1` and `b0 b1`, without building them.
fn joined_cmp(a0: &[u8], a1: &[u8], b0: &[u8], b1: &[u8]) -> Ordering {
    if a0 == b0 {
        return path_cmp(a1, b1);
    }
    let (mut x, mut x_next) = (a0, a1);
    let (mut y, mut y_next) = (b0, b1);
    loop {
        if x.is_empty() {
            if x_next.is_empty() {
                return if y.is_empty() && y_next.is_empty() { Ordering::Equal } else { Ordering::Less };
            }
            (x, x_next) = (x_next, &[]);
            continue;
        }
        if y.is_empty() {
            if y_next.is_empty() {
                return Ordering::Greater;
            }
            (y, y_next) = (y_next, &[]);
            continue;
        }
        if let Some(i) = first_diff(x, y) {
            return byte_key(x[i]).cmp(&byte_key(y[i]));
        }
        let n = x.len().min(y.len());
        x = &x[n..];
        y = &y[n..];
    }
}

/// File name of a project unit's metadata ([`WalkOptions::projects`]).
const PYPROJECT: &[u8] = crate::config::PYPROJECT_FILE.as_bytes();

/// Lockfiles: generated, large, and full of package names that are not code references.
pub const LOCKFILES: &[&[u8]] = &[
    b"uv.lock",
    b"poetry.lock",
    b"pdm.lock",
    b"Pipfile.lock",
    b"package-lock.json",
    b"npm-shrinkwrap.json",
    b"yarn.lock",
    b"pnpm-lock.yaml",
    b"Cargo.lock",
    b"composer.lock",
];

/// Whether file name `name` is a lockfile ([`LOCKFILES`]).
pub fn is_lockfile(name: &[u8]) -> bool {
    LOCKFILES.contains(&name)
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
/// Paths below a root are `out + rel`. Gitignore matching uses the *match key*
/// `key_prefix + rel`, which is the path relative to the enclosing repository top (or to the
/// root when there is none), so ancestor ignore files see the paths they expect. Configured
/// excludes use `ex_prefix + rel`, the path relative to [`WalkOptions::exclude_base`].
struct RootCtx {
    /// Output prefix: the root path plus `/`, or empty for `.`.
    out: Box<[u8]>,
    key_prefix: Box<[u8]>,
    ex_prefix: Box<[u8]>,
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
    ex_key: Vec<u8>,
    found: Found,
    /// Full paths of the `pyproject.toml` files found ([`WalkOptions::projects`]; rare).
    projects: Vec<Box<[u8]>>,
    /// Full paths of the config files found ([`WalkOptions::configs`]).
    configs: Vec<Box<[u8]>>,
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
    let mut main = Worker::default();
    for pat in &opts.excludes {
        if let Err(e) = Gitignore::check_pattern(pat.as_bytes()) {
            let error = io::Error::new(io::ErrorKind::InvalidInput, e);
            main.diag(DiagnosticKind::Exclude, Path::new(pat), error);
        }
    }
    let excludes = Gitignore::from_patterns(&opts.excludes);
    let excludes = if excludes.is_empty() { None } else { Some(excludes) };
    let base = match &opts.exclude_base {
        Some(b) if excludes.is_some() => Some(abs_normal(b)),
        _ => None,
    };
    let mut ctxs = Vec::with_capacity(roots.len());
    let mut seeds = Vec::with_capacity(roots.len());
    let mut file_roots = Vec::new();
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
            if meta.is_file() {
                file_roots.push(root);
            }
            continue;
        }
        let out = root_prefix(root.as_os_str().as_bytes());
        let (key_prefix, ignore) = if opts.gitignore && opts.parents {
            parent_chain(root, &mut main)
        } else {
            (Box::default(), None)
        };
        let ex_prefix = match &base {
            Some(b) => rel_prefix(root, b),
            None => Box::default(),
        };
        seeds.push(DirTask { path: out.clone(), root: ctxs.len() as u32, ignore });
        ctxs.push(RootCtx { out, key_prefix, ex_prefix });
    }
    // File roots are stored whole, under an extra root index with an empty prefix.
    let file_root = ctxs.len() as u32;
    for root in file_roots {
        if main.found.push(file_root, &[root.as_os_str().as_bytes()]).is_err() {
            main.diag(DiagnosticKind::PathListFull, root, io::Error::other(PathListFull));
        }
    }
    let sh = Shared { opts, roots: ctxs, excludes, workers: (0..threads).map(|_| Mutex::default()).collect() };
    rayon::scope(|s| {
        for task in seeds {
            let sh = &sh;
            s.spawn(move |s| run_dir(s, sh, task));
        }
    });
    let mut found = Vec::with_capacity(sh.workers.len() + 1);
    found.push(main.found);
    let mut out = Walk {
        files: PathList::default(),
        projects: PathList::default(),
        configs: PathList::default(),
        diagnostics: main.diags,
        stats: main.stats,
    };
    let mut projects: Vec<Box<[u8]>> = Vec::new();
    let mut configs: Vec<Box<[u8]>> = Vec::new();
    for slot in sh.workers {
        let w = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
        out.stats.add(&w.stats);
        out.diagnostics.extend(w.diags);
        found.push(w.found);
        projects.extend(w.projects);
        configs.extend(w.configs);
    }
    projects.sort_unstable_by(|a, b| path_cmp(a, b));
    projects.dedup();
    for p in &projects {
        // Fewer bytes than the source paths: cannot overflow before them.
        let _ = out.projects.push(p);
    }
    configs.sort_unstable_by(|a, b| path_cmp(a, b));
    configs.dedup();
    for p in &configs {
        if out.configs.push(p).is_err() {
            break;
        }
    }
    let mut prefixes: Vec<&[u8]> = sh.roots.iter().map(|r| &*r.out).collect();
    prefixes.push(b"");
    let (files, dropped) = merge(found, &prefixes, opts.sort);
    out.files = files;
    for _ in 0..dropped {
        out.diagnostics.push(Diagnostic {
            kind: DiagnosticKind::PathListFull,
            path: PathBuf::new(),
            error: io::Error::other(PathListFull),
        });
    }
    if opts.sort {
        out.diagnostics.sort_by(|a, b| {
            path_cmp(a.path.as_os_str().as_bytes(), b.path.as_os_str().as_bytes())
                .then((a.kind as u8).cmp(&(b.kind as u8)))
        });
    }
    out
}

/// `path` made absolute against the working directory and lexically normalised.
fn abs_normal(path: &Path) -> PathBuf {
    match std::path::absolute(path) {
        Ok(p) => crate::config::normalize(&p),
        Err(_) => crate::config::normalize(path),
    }
}

/// `root` relative to the exclude base `base` plus `/` (empty when `root` is `base`). Tried
/// lexically first, then with symlinks resolved; a root outside `base` gets an empty prefix.
fn rel_prefix(root: &Path, base: &Path) -> Box<[u8]> {
    let lexical = abs_normal(root);
    let rel = match lexical.strip_prefix(base) {
        Ok(r) => r.to_path_buf(),
        Err(_) => match (fs::canonicalize(root), fs::canonicalize(base)) {
            (Ok(r), Ok(b)) => match r.strip_prefix(&b) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => return Box::default(),
            },
            _ => return Box::default(),
        },
    };
    let rel = rel.as_os_str().as_bytes();
    if rel.is_empty() {
        return Box::default();
    }
    let mut v = Vec::with_capacity(rel.len() + 1);
    v.extend_from_slice(rel);
    v.push(b'/');
    v.into_boxed_slice()
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
        // `.ignore` goes on top: it beats `.gitignore` of the same directory.
        for name in [".gitignore", ".ignore"] {
            let file = dir.join(name);
            // Like git, an ignore file that is a symlink is not read.
            if fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                chain = load_ignore(&file, base, chain, w);
            }
        }
        dir.push(bytes_path(comp));
        base += comp.len() + 1;
    }
    let mut key_prefix = Vec::with_capacity(rel.len() + 1);
    key_prefix.extend_from_slice(rel);
    key_prefix.push(b'/');
    (key_prefix.into_boxed_slice(), chain)
}

/// Whether the entry `name` is excluded by the configured patterns. `key[..dir_len]` holds the
/// directory part of the exclude key.
#[inline]
fn is_excluded(ex: &Gitignore, key: &mut Vec<u8>, dir_len: usize, name: &[u8], is_dir: bool) -> bool {
    key.truncate(dir_len);
    key.extend_from_slice(name);
    ex.matched(key, is_dir) == Match::Ignore
}

/// Whether the entry `name` is ignored by the ignore-file chain. `key[..dir_len]` holds the
/// directory part of the match key.
fn is_gitignored(key: &mut Vec<u8>, dir_len: usize, name: &[u8], is_dir: bool, chain: &Arc<IgnoreNode>) -> bool {
    key.truncate(dir_len);
    key.extend_from_slice(name);
    let mut node = Some(chain);
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
    let mut has_ignore = false;
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
            } else if name == b".ignore" && kind == Kind::File {
                has_ignore = true;
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
    let rel = &task.path[ctx.out.len()..];
    let dir_key_len = ctx.key_prefix.len() + rel.len();
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
    // `.ignore` goes on top: it beats `.gitignore` of the same directory.
    if has_ignore {
        chain = load_ignore(&dir.join(".ignore"), dir_key_len, chain, w);
    }
    if chain.is_some() {
        w.key.clear();
        w.key.extend_from_slice(&ctx.key_prefix);
        w.key.extend_from_slice(rel);
    }
    let excludes = sh.excludes.as_ref();
    let ex_dir_len = ctx.ex_prefix.len() + rel.len();
    if excludes.is_some() {
        w.ex_key.clear();
        w.ex_key.extend_from_slice(&ctx.ex_prefix);
        w.ex_key.extend_from_slice(rel);
    }
    let ignored = |w_key: &mut Vec<u8>, ex_key: &mut Vec<u8>, name: &[u8], is_dir: bool| {
        excludes.is_some_and(|ex| is_excluded(ex, ex_key, ex_dir_len, name, is_dir))
            || chain.as_ref().is_some_and(|c| is_gitignored(w_key, dir_key_len, name, is_dir, c))
    };

    // Pass 2: classify.
    for &Ent { start, end, kind } in &w.ents {
        let name = &w.names[start as usize..end as usize];
        match kind {
            Kind::File => {
                if !is_python(name) {
                    if opts.projects && name == PYPROJECT && !ignored(&mut w.key, &mut w.ex_key, name, false) {
                        w.projects.push([&task.path[..], name].concat().into_boxed_slice());
                    }
                    if !opts.configs.is_empty()
                        && !is_lockfile(name)
                        && opts.configs.iter().any(|g| wildmatch::wildmatch(g.as_bytes(), name))
                        && !ignored(&mut w.key, &mut w.ex_key, name, false)
                    {
                        w.configs.push([&task.path[..], name].concat().into_boxed_slice());
                    }
                    continue;
                }
                if ignored(&mut w.key, &mut w.ex_key, name, false) {
                    w.stats.ignored += 1;
                    continue;
                }
                if w.found.push(task.root, &[rel, name]).is_err() {
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
                if ignored(&mut w.key, &mut w.ex_key, name, true) {
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
