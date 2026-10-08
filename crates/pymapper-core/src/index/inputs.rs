//! What to load: the seam between import roots and the loader.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::FileId;
use crate::intern::{FxHashMap, FxHashSet};
use crate::roots::{DistId, FileStatus, Origin, Roots, is_identifier};
use crate::walk::{PathList, PathListFull};

/// A distribution node of the hierarchy: the first-party project, the stdlib, or one
/// third-party distribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: Box<[u8]>,
    pub origin: Origin,
    pub dist: Option<DistId>,
}

/// Files to load, in FileId order, with their group, dotted module name and status.
///
/// Build it with [`Inputs::from_root_dir`] / [`Inputs::from_roots`], or by hand with
/// [`Inputs::add_group`] + [`Inputs::push`]. The order of files is the FileId order and is
/// kept by the index; give a deterministic order (the walker's is).
#[derive(Default, Debug)]
pub struct Inputs {
    pub(crate) paths: PathList,
    /// Dotted module names, parallel to `paths`: `mod_bytes[mod_ends[i - 1]..mod_ends[i]]`.
    mod_bytes: Vec<u8>,
    mod_ends: Vec<usize>,
    pub(crate) group: Vec<u32>,
    pub(crate) status: Vec<FileStatus>,
    pub(crate) groups: Vec<Group>,
}

impl Inputs {
    pub fn new() -> Inputs {
        Inputs::default()
    }

    /// Adds a group; returns its index.
    pub fn add_group(&mut self, name: &[u8], origin: Origin, dist: Option<DistId>) -> u32 {
        self.groups.push(Group { name: name.into(), origin, dist });
        (self.groups.len() - 1) as u32
    }

    /// Adds a file: its path, group, dotted module name (non-empty) and status.
    ///
    /// # Panics
    /// If `group` was not added.
    pub fn push(&mut self, path: &[u8], group: u32, module: &[u8], status: FileStatus) -> Result<FileId, PathListFull> {
        assert!((group as usize) < self.groups.len(), "unknown group {group}");
        self.paths.push(path)?;
        self.mod_bytes.extend_from_slice(module);
        self.mod_ends.push(self.mod_bytes.len());
        self.group.push(group);
        self.status.push(status);
        Ok((self.group.len() - 1) as FileId)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.group.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.group.is_empty()
    }

    #[inline]
    pub fn path(&self, f: FileId) -> &Path {
        self.paths.get(f as usize)
    }

    #[inline]
    pub fn module(&self, f: FileId) -> &[u8] {
        let i = f as usize;
        let start = if i == 0 { 0 } else { self.mod_ends[i - 1] };
        &self.mod_bytes[start..self.mod_ends[i]]
    }

    #[inline]
    pub fn status(&self, f: FileId) -> FileStatus {
        self.status[f as usize]
    }

    #[inline]
    pub fn group(&self, f: FileId) -> u32 {
        self.group[f as usize]
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Every file of `files` in one group named `name`, module names derived from the path
    /// relative to `root` (the import root): `a/b.py` -> `a.b`, `a/__init__.py` -> `a`.
    ///
    /// Statuses: a `.py` next to a `.pyi` of the same module is [`FileStatus::StubShadowed`];
    /// a path with a non-identifier component (or the root's own `__init__.py`) is
    /// [`FileStatus::NotImportable`] (its name is still derived from the path); a file not under
    /// `root` is [`FileStatus::Outside`] (named after its file stem). The finer import-system
    /// rules (package beats module, several roots) need [`Inputs::from_roots`].
    pub fn from_root_dir(root: &Path, files: &PathList, origin: Origin, name: &[u8]) -> Inputs {
        let mut inp = Inputs::new();
        let g = inp.add_group(name, origin, None);
        let rb = root.as_os_str().as_bytes();
        let rb = rb.strip_suffix(b"/").unwrap_or(rb);
        let mut buf = Vec::new();
        for i in 0..files.len() {
            let p = files.get_bytes(i);
            let status = match p.strip_prefix(rb).and_then(|r| r.strip_prefix(b"/")) {
                Some(rel) => module_from_rel(rel, &mut buf),
                None => {
                    module_from_rel(file_name(p), &mut buf);
                    FileStatus::Outside
                }
            };
            // A full path list means the input itself exceeds 4 GiB of paths; drop the rest.
            if inp.push(p, g, &buf, status).is_err() {
                break;
            }
        }
        inp.mark_stub_shadowed();
        inp
    }

    /// Every file of `files` classified by the discovered import roots: module names and
    /// statuses from [`Roots::module_map`] (PEP 420 / PEP 561), one group per distribution
    /// (first-party code is one group named after the project, the stdlib one group named
    /// `stdlib`, third-party files without a distribution one group named `unknown`).
    pub fn from_roots(roots: &Roots, files: &PathList) -> Inputs {
        let map = roots.module_map(files);
        let mut inp = Inputs::new();
        let mut by_key: FxHashMap<(Origin, Option<u32>), u32> = FxHashMap::default();
        let mut buf = Vec::new();
        for i in 0..files.len() {
            let p = files.get(i);
            let loc = roots.classify(p);
            let origin = loc.map_or(Origin::FirstParty, |l| l.origin);
            let dist = if origin == Origin::ThirdParty { loc.and_then(|l| l.dist) } else { None };
            let g = *by_key.entry((origin, dist.map(|d| d.0))).or_insert_with(|| {
                let name: Vec<u8> = match (origin, dist) {
                    (Origin::FirstParty, _) => project_name(roots),
                    (Origin::Stdlib, _) => b"stdlib".to_vec(),
                    (Origin::ThirdParty, Some(d)) => roots.dists.normalized_name(d).to_vec(),
                    (Origin::ThirdParty, None) => b"unknown".to_vec(),
                };
                inp.add_group(&name, origin, dist)
            });
            let status = map.file_status(i as u32);
            buf.clear();
            match (map.file_module_name(i as u32), loc) {
                (Some(m), _) => buf.extend_from_slice(m),
                (None, Some(l)) if !l.single => {
                    let prefix = &roots.root(l.root).prefix;
                    if !prefix.is_empty() {
                        buf.extend_from_slice(prefix);
                        buf.push(b'.');
                    }
                    let mut rest = Vec::new();
                    module_from_rel(l.rel, &mut rest);
                    buf.extend_from_slice(&rest);
                }
                (None, Some(l)) => buf.extend_from_slice(&roots.root(l.root).prefix),
                (None, None) => {
                    module_from_rel(file_name(p.as_os_str().as_bytes()), &mut buf);
                }
            }
            if buf.is_empty() {
                module_from_rel(file_name(p.as_os_str().as_bytes()), &mut buf);
            }
            if inp.push(p.as_os_str().as_bytes(), g, &buf, status).is_err() {
                break;
            }
        }
        inp
    }

    /// Marks every active `.py` whose module also has an active `.pyi` as stub-shadowed.
    fn mark_stub_shadowed(&mut self) {
        let Inputs { paths, mod_bytes, mod_ends, group, status, .. } = self;
        let module = |i: usize| &mod_bytes[if i == 0 { 0 } else { mod_ends[i - 1] }..mod_ends[i]];
        let mut stubs: FxHashSet<(u32, &[u8])> = FxHashSet::default();
        for i in 0..group.len() {
            if status[i] == FileStatus::Active && paths.get_bytes(i).ends_with(b".pyi") {
                stubs.insert((group[i], module(i)));
            }
        }
        if stubs.is_empty() {
            return;
        }
        for i in 0..group.len() {
            if status[i] == FileStatus::Active
                && paths.get_bytes(i).ends_with(b".py")
                && stubs.contains(&(group[i], module(i)))
            {
                status[i] = FileStatus::StubShadowed;
            }
        }
    }
}

/// Last `/`-separated component.
fn file_name(p: &[u8]) -> &[u8] {
    p.rsplit(|&b| b == b'/').next().unwrap_or(p)
}

/// `pkg/__init__.py[i]` (or a bare `__init__.py[i]`).
pub(crate) fn is_init(path: &[u8]) -> bool {
    let n = file_name(path);
    n == b"__init__.py" || n == b"__init__.pyi"
}

/// Writes the dotted module name of a root-relative path into `out` and returns its status
/// ([`FileStatus::Active`] or [`FileStatus::NotImportable`]).
pub(crate) fn module_from_rel(rel: &[u8], out: &mut Vec<u8>) -> FileStatus {
    out.clear();
    let stem = rel.strip_suffix(b".pyi").or_else(|| rel.strip_suffix(b".py"));
    let mut importable = stem.is_some();
    let mut stem = stem.unwrap_or(rel);
    if let Some(pkg) = stem.strip_suffix(b"/__init__") {
        stem = pkg;
    } else if stem == b"__init__" {
        // The root's own `__init__.py` is not a module of this root.
        importable = false;
    }
    for (k, comp) in stem.split(|&b| b == b'/').enumerate() {
        if k > 0 {
            out.push(b'.');
        }
        importable &= is_identifier(comp);
        out.extend_from_slice(comp);
    }
    if importable { FileStatus::Active } else { FileStatus::NotImportable }
}

/// Name of the first-party group: `[project] name`, else the project directory name.
fn project_name(roots: &Roots) -> Vec<u8> {
    if let Some(n) = roots.pyproject.as_ref().and_then(|p| p.name.as_deref()) {
        return n.as_bytes().to_vec();
    }
    match roots.project_root.file_name() {
        Some(n) => n.as_bytes().to_vec(),
        None => b"project".to_vec(),
    }
}
