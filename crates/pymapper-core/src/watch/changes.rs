//! From a [`Batch`] of raw paths to what an incremental update needs: which loaded files
//! changed contents, whether the file set may have changed (walk again), and whether the
//! configuration may have changed (load again).
//!
//! ```text
//!   path (relative to the watched root)           ->  outcome
//!   any component `.git`                              ignored
//!   pyproject.toml, pymapper.toml                     reload   (config, project units, roots)
//!   .gitignore, .ignore, .git/info/exclude            rewalk   (the walk's file set)
//!   loaded first-party .py/.pyi that exists           changed  (re-extract)
//!   loaded first-party .py/.pyi that is gone          rewalk
//!   other .py/.pyi that exists, outside hidden dirs   rewalk   (a new file, unless excluded)
//!   and __pycache__
//!   directory event outside hidden dirs               rewalk   (moved / created / deleted trees)
//!   anything else (temp files, caches, gone files)    ignored
//!   batch.rescan()                                    rewalk + every first-party file changed
//! ```
//!
//! A rewalk is conservative: when the walk finds the same files, the update plans nothing
//! for them (same paths, no changed contents). Only first-party files are tracked: edits of
//! third-party / stdlib files on disk are not picked up (their distributions are loaded
//! from their `RECORD`s; reload to see a new install).
//!
//! Cost per batch: one `lstat` per Python path, and membership tests against the loaded
//! first-party paths: a scan per path for small batches (up to 8 Python paths: O(files) byte
//! comparisons each), else one hash set over the paths (borrowed from the index).

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::queue::{Batch, FLAG_DIR};
use crate::incremental::{IndexDiff, Program, UpdateStats};
use crate::index::{Index, Inputs};
use crate::intern::FxHashSet;
use crate::roots::Origin;
use crate::walk::is_python;

/// What a batch means for the loaded program (see the module docs).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes<'b> {
    /// Loaded files whose contents changed (paths as in the index), sorted.
    pub changed: Vec<&'b Path>,
    /// The file set may have changed: walk again and pass the new inputs to the update.
    pub rewalk: bool,
    /// Configuration files changed: load again from scratch.
    pub reload: bool,
    /// Events were lost: treat every first-party file as changed (implies `rewalk`).
    pub all: bool,
    /// Paths that did not matter.
    pub ignored: u32,
}

impl Changes<'_> {
    /// Nothing to update.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && !self.rewalk && !self.reload && !self.all
    }
}

/// Files named like this change the configuration.
const CONFIG_NAMES: [&[u8]; 2] = [b"pyproject.toml", b"pymapper.toml"];
/// Files named like this change what the walker lists.
const IGNORE_NAMES: [&[u8]; 2] = [b".gitignore", b".ignore"];

/// Classifies `batch` against the files loaded in `ix`. `root`: the watched directory the
/// batch's paths lie under (components above it are not inspected, so a project inside a
/// hidden directory works).
pub fn classify<'b>(batch: &'b Batch, ix: &Index, root: &Path) -> Changes<'b> {
    let mut out = Changes::default();
    if batch.rescan() {
        out.rewalk = true;
        out.all = true;
    }
    let root = root.as_os_str().as_bytes();
    let known = Known::new(ix, batch);
    for i in 0..batch.len() {
        let p = batch.path_bytes(i);
        let rel = match p.strip_prefix(root) {
            Some([b'/', rest @ ..]) => rest,
            Some([]) => &[][..],
            _ => p,
        };
        let name = rel.rsplit(|&c| c == b'/').next().unwrap_or(rel);
        let mut comps = rel.split(|&c| c == b'/');
        if comps.any(|c| c == b".git") {
            if rel.ends_with(b".git/info/exclude") {
                out.rewalk = true;
            } else {
                out.ignored += 1;
            }
            continue;
        }
        if CONFIG_NAMES.contains(&name) {
            out.reload = true;
            continue;
        }
        if IGNORE_NAMES.contains(&name) {
            out.rewalk = true;
            continue;
        }
        let path = batch.path(i);
        if batch.flags(i) & FLAG_DIR == 0 && is_python(name) {
            let exists = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
            match (known.contains(p), exists) {
                (true, true) => out.changed.push(path),
                (true, false) => out.rewalk = true,
                (false, true) if !hidden(rel) => out.rewalk = true,
                _ => out.ignored += 1,
            }
            continue;
        }
        let dir = batch.flags(i) & FLAG_DIR != 0 || std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if dir && !rel.is_empty() && !hidden(rel) {
            out.rewalk = true;
        } else {
            out.ignored += 1;
        }
    }
    out
}

/// A relative path with a component the first-party walk skips by default: hidden
/// (`.venv`, `.mypy_cache`) or `__pycache__`.
fn hidden(rel: &[u8]) -> bool {
    rel.split(|&c| c == b'/').any(|c| c.first() == Some(&b'.') || c == b"__pycache__")
}

/// Batches with at most this many Python paths look them up by scanning the first-party
/// paths (a few memcmps per file) instead of hashing every path first.
const SCAN_MAX: usize = 8;

/// Membership in the loaded first-party paths.
enum Known<'a> {
    Scan(&'a Index),
    Set(FxHashSet<&'a [u8]>),
}

impl<'a> Known<'a> {
    fn new(ix: &'a Index, batch: &Batch) -> Known<'a> {
        let python = (0..batch.len()).filter(|&i| batch.flags(i) & FLAG_DIR == 0 && is_python(batch.path_bytes(i))).count();
        if python <= SCAN_MAX {
            return Known::Scan(ix);
        }
        let mut set = FxHashSet::default();
        for f in 0..ix.file_count() {
            if ix.file_origin(f) == Origin::FirstParty {
                set.insert(ix.file_path(f).as_os_str().as_bytes());
            }
        }
        Known::Set(set)
    }

    fn contains(&self, p: &[u8]) -> bool {
        match self {
            Known::Set(s) => s.contains(p),
            Known::Scan(ix) => (0..ix.file_count()).any(|f| ix.file_origin(f) == Origin::FirstParty && ix.file_path(f).as_os_str().as_bytes() == p),
        }
    }
}

/// The inputs `ix` was loaded from (same files, order, groups, module names and statuses):
/// what [`crate::incremental::Program::update`] takes when only contents changed. O(files),
/// no I/O.
pub fn inputs_of(ix: &Index) -> Inputs {
    let mut inputs = Inputs::new();
    for g in ix.groups() {
        inputs.add_group(&g.name, g.origin, g.dist);
    }
    for f in 0..ix.file_count() {
        inputs
            .push(ix.file_path(f).as_os_str().as_bytes(), ix.file_group(f), ix.str(ix.file_module(f)), ix.file_status(f))
            .expect("the paths of a loaded index fit a path list");
    }
    inputs
}

/// Applies `changes` to `program`: the inputs are `rewalk()` when the file set may have
/// changed (else [`inputs_of`] the current index), the changed files those of `changes`
/// (every first-party file of the new inputs after a rescan). `changes.reload` is the
/// caller's business (reload from scratch instead of calling this).
pub fn update(program: &mut Program, changes: &Changes<'_>, rewalk: impl FnOnce() -> Inputs) -> (IndexDiff, UpdateStats) {
    let inputs = if changes.rewalk || changes.all { rewalk() } else { inputs_of(program.index()) };
    if !changes.all {
        return program.update(inputs, &changes.changed);
    }
    let first_party: Vec<u32> = inputs.groups().iter().enumerate().filter(|(_, g)| g.origin == Origin::FirstParty).map(|(i, _)| i as u32).collect();
    let all: Vec<std::path::PathBuf> =
        (0..inputs.len() as u32).filter(|&f| first_party.contains(&inputs.group(f))).map(|f| inputs.path(f).to_path_buf()).collect();
    program.update(inputs, &all)
}
