//! Project units: the project root and every directory below it that holds a `pyproject.toml`
//! (nested ones included). A monorepo is one project made of several units; dead-code
//! `mode = "auto"` picks library or application mode per unit (see `reach::FileModes`).

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::project::read_pyproject;
use super::{Diagnostic, DiagnosticKind, PyprojectInfo, is_under};
use crate::config::{PYPROJECT_FILE, normalize};
use crate::intern::FxHashMap;
use crate::walk::PathList;

/// The units of a project. Unit 0 is the project root (with or without a `pyproject.toml`);
/// every first-party file belongs to its nearest enclosing unit, files outside any other unit
/// (and outside the project root) to unit 0.
#[derive(Clone, Debug)]
pub struct ProjectUnits {
    /// Unit directories, absolute and normalised; `[0]` is the project root.
    dirs: Vec<PathBuf>,
    /// Directory bytes -> unit, for every unit but the root.
    by_dir: FxHashMap<Box<[u8]>, u32>,
    /// Script targets of every unit (`[project.scripts]`, `gui-scripts`, `entry-points`),
    /// grouped by unit: unit `u` owns `scripts[starts[u]..starts[u + 1]]`.
    scripts: Vec<String>,
    starts: Vec<u32>,
}

impl ProjectUnits {
    /// A project of one unit, the root, with the script targets of its parsed `pyproject`.
    pub fn single(root: &Path, pyproject: Option<&PyprojectInfo>) -> ProjectUnits {
        let scripts = pyproject.map(|p| p.scripts.clone()).unwrap_or_default();
        let starts = vec![0, scripts.len() as u32];
        ProjectUnits { dirs: vec![normalize(root)], by_dir: FxHashMap::default(), scripts, starts }
    }

    /// The units of the project at `root`: the root, with its already parsed `pyproject`
    /// ([`Roots::pyproject`](super::Roots::pyproject)), plus the directory of each file of
    /// `pyprojects` strictly below it (others are ignored). Those are parsed here, once each; an
    /// unreadable or invalid one still makes a unit, without scripts, and is reported. O(units).
    pub fn discover(root: &Path, pyproject: Option<&PyprojectInfo>, pyprojects: &PathList) -> (ProjectUnits, Vec<Diagnostic>) {
        let mut units = ProjectUnits::single(root, pyproject);
        let mut diags = Vec::new();
        for path in pyprojects.iter() {
            let Some(dir) = path.parent().map(normalize) else { continue };
            if dir == units.dirs[0] || !is_under(&dir, &units.dirs[0]) || units.by_dir.contains_key(dir.as_os_str().as_bytes()) {
                continue;
            }
            match read_pyproject(&dir) {
                Ok(info) => units.scripts.extend(info.into_iter().flat_map(|i| i.scripts)),
                Err(message) => diags.push(Diagnostic { kind: DiagnosticKind::Pyproject, path: dir.join(PYPROJECT_FILE), message }),
            }
            units.starts.push(units.scripts.len() as u32);
            units.by_dir.insert(dir.as_os_str().as_bytes().into(), units.dirs.len() as u32);
            units.dirs.push(dir);
        }
        (units, diags)
    }

    /// Number of units (at least 1).
    #[inline]
    pub fn len(&self) -> u32 {
        self.dirs.len() as u32
    }

    /// Always false: the root is a unit.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Directory of unit `u`.
    pub fn dir(&self, u: u32) -> &Path {
        &self.dirs[u as usize]
    }

    /// Script targets declared by unit `u`'s `pyproject.toml`.
    pub fn scripts(&self, u: u32) -> &[String] {
        &self.scripts[self.starts[u as usize] as usize..self.starts[u as usize + 1] as usize]
    }

    /// Script targets of every unit.
    pub fn all_scripts(&self) -> &[String] {
        &self.scripts
    }

    /// The unit of the directory `dir` (absolute, normalised): the nearest enclosing unit, else
    /// 0. One hash lookup per ancestor of `dir`, none for a single-unit project.
    pub fn unit_of_dir(&self, dir: &[u8]) -> u32 {
        if self.by_dir.is_empty() {
            return 0;
        }
        let mut d = dir;
        loop {
            if let Some(&u) = self.by_dir.get(d) {
                return u;
            }
            match d.iter().rposition(|&b| b == b'/') {
                Some(i) if i > 0 => d = &d[..i],
                _ => return 0,
            }
        }
    }
}
