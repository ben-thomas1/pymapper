//! Project units: the project root and every directory below it that holds a `pyproject.toml`
//! (nested ones included). A monorepo is one project made of several units; dead-code
//! `mode = "auto"` picks library or application mode per unit (see `reach::FileModes`).
//!
//! Units also contribute first-party import roots ([`ProjectUnits::import_dirs`]): a uv
//! workspace member `src/lib-a/` with `src/lib-a/pkg/` is imported as `pkg`, not as
//! `src.lib-a.pkg`, whether or not a venv with editable installs exists.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::project::{is_src_layout, read_pyproject};
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
    /// First-party import roots of the units below the root, in discovery order, and the unit
    /// of each.
    import_dirs: Vec<PathBuf>,
    import_unit: Vec<u32>,
    /// Per unit: how many of its scripts are commands ([`PyprojectInfo::app_scripts`]).
    app_scripts: Vec<u32>,
}

impl ProjectUnits {
    /// A project of one unit, the root, with the script targets of its parsed `pyproject`.
    pub fn single(root: &Path, pyproject: Option<&PyprojectInfo>) -> ProjectUnits {
        let scripts = pyproject.map(|p| p.scripts.clone()).unwrap_or_default();
        let starts = vec![0, scripts.len() as u32];
        let app_scripts = vec![pyproject.map_or(0, |p| p.app_scripts)];
        ProjectUnits {
            dirs: vec![normalize(root)],
            by_dir: FxHashMap::default(),
            scripts,
            starts,
            import_dirs: Vec::new(),
            import_unit: Vec::new(),
            app_scripts,
        }
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
            let before = units.import_dirs.len();
            let mut app = 0;
            match read_pyproject(&dir) {
                Ok(info) => {
                    unit_import_dirs(&dir, info.as_ref(), &mut units.import_dirs);
                    app = info.as_ref().map_or(0, |i| i.app_scripts);
                    units.scripts.extend(info.into_iter().flat_map(|i| i.scripts));
                }
                Err(message) => {
                    unit_import_dirs(&dir, None, &mut units.import_dirs);
                    diags.push(Diagnostic { kind: DiagnosticKind::Pyproject, path: dir.join(PYPROJECT_FILE), message });
                }
            }
            let unit = units.dirs.len() as u32;
            units.import_unit.resize(units.import_dirs.len().max(before), unit);
            units.app_scripts.push(app);
            units.starts.push(units.scripts.len() as u32);
            units.by_dir.insert(dir.as_os_str().as_bytes().into(), units.dirs.len() as u32);
            units.dirs.push(dir);
        }
        (units, diags)
    }

    /// First-party import roots contributed by the units below the root: per unit, its
    /// build-backend source directories, its `src/` layout, then the unit directory itself, as
    /// for the project root ([`discover`](super::discover)). A unit directory that is itself a
    /// package (`__init__.py`, e.g. the `pyproject.toml` shipped inside a wheel's package) adds
    /// nothing: its files keep the names the enclosing root gives them.
    pub fn import_dirs(&self) -> &[PathBuf] {
        &self.import_dirs
    }

    /// The unit each of [`import_dirs`](Self::import_dirs) belongs to.
    pub fn import_dir_units(&self) -> &[u32] {
        &self.import_unit
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

    /// How many of unit `u`'s script targets are commands (`[project.scripts]`,
    /// `[project.gui-scripts]`) rather than plugin entry points.
    pub fn app_scripts(&self, u: u32) -> u32 {
        self.app_scripts.get(u as usize).copied().unwrap_or(0)
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

/// Appends the import roots of the unit at `dir` (see [`ProjectUnits::import_dirs`]).
fn unit_import_dirs(dir: &Path, info: Option<&PyprojectInfo>, out: &mut Vec<PathBuf>) {
    if dir.join("__init__.py").exists() || dir.join("__init__.pyi").exists() {
        return;
    }
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for d in info.map_or(&[][..], |i| &i.source_dirs[..]) {
        let p = normalize(&dir.join(d));
        if p != dir && is_under(&p, dir) && p.is_dir() {
            push(p);
        }
    }
    let src = dir.join("src");
    if is_src_layout(&src) {
        push(src);
    }
    push(dir.to_path_buf());
}
