//! First-party layout: project root (`pyproject.toml`), `src` layout and build-backend hints.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use toml::Spanned;
use toml::de::{DeTable, DeValue};

use super::is_identifier;
use super::pyvenv::parse_version_prefix;
use crate::config::{PYPROJECT_FILE, PythonVersion, STANDALONE_FILE, normalize};

/// The project root for a run that starts in the directory `start` (absolute, normalised),
/// independent of how deep below the root `start` is. Walking up from `start` (inclusive):
///
/// 1. the outermost directory whose `pyproject.toml` has a `[tool.uv.workspace]` table;
/// 2. else the top of the git repository: the nearest directory holding `.git` (a directory,
///    or the file of a worktree / submodule);
/// 3. else the outermost directory with a `pyproject.toml` or `pymapper.toml`;
/// 4. else `start`.
///
/// The walk stops at the first `.git` boundary (a project never spans repositories). When
/// `start` lies strictly below `home`, `home` and its ancestors are not considered: a stray
/// `~/pyproject.toml` must not swallow every project. One `stat` per ancestor plus one read of
/// each `pyproject.toml` found.
pub fn find_project_root(start: &Path, home: Option<&Path>) -> PathBuf {
    let home = home.filter(|h| start != *h && start.starts_with(h));
    let mut workspace: Option<&Path> = None;
    let mut marker: Option<&Path> = None;
    for dir in start.ancestors() {
        if home.is_some_and(|h| dir == h) {
            break;
        }
        let pyproject = dir.join(PYPROJECT_FILE);
        if pyproject.is_file() {
            marker = Some(dir);
            if fs::read_to_string(&pyproject).is_ok_and(|t| has_uv_workspace(&t)) {
                workspace = Some(dir);
            }
        } else if dir.join(STANDALONE_FILE).is_file() {
            marker = Some(dir);
        }
        if dir.join(".git").exists() {
            return workspace.unwrap_or(dir).to_path_buf();
        }
    }
    workspace.or(marker).unwrap_or(start).to_path_buf()
}

/// Whether a `pyproject.toml` declares a uv workspace (`[tool.uv.workspace]`).
pub fn has_uv_workspace(text: &str) -> bool {
    // Cheap prefilter: most files never mention it.
    if !text.contains("workspace") {
        return false;
    }
    DeTable::parse(text).is_ok_and(|doc| table(doc.get_ref(), &["tool", "uv", "workspace"]).is_some())
}

/// What we use from `pyproject.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PyprojectInfo {
    /// `[project] name`.
    pub name: Option<String>,
    /// Lowest `3.N` allowed by `[project] requires-python`.
    pub requires_python: Option<PythonVersion>,
    /// Import roots declared to the build backend, relative to the project root, in the order
    /// found: setuptools `package-dir` / `packages.find.where`, poetry `packages.from`, hatch
    /// wheel `packages` parents, uv `module-root`, pdm `package-dir`, maturin `python-source`.
    pub source_dirs: Vec<PathBuf>,
    /// Entry point targets (`pkg.mod:attr`, extras kept verbatim) of `[project.scripts]`,
    /// `[project.gui-scripts]` and every group of `[project.entry-points]`, in file order.
    pub scripts: Vec<String>,
    /// How many of `scripts` (the first ones) come from `[project.scripts]` /
    /// `[project.gui-scripts]`: commands, i.e. an application (plugin entry-point groups are
    /// roots too, but libraries declare them).
    pub app_scripts: u32,
}

type Table<'i> = DeTable<'i>;

fn get<'a, 'i>(t: &'a Table<'i>, key: &str) -> Option<&'a DeValue<'i>> {
    t.iter().find(|(k, _)| k.get_ref() == key).map(|(_, v)| v.get_ref())
}

fn table<'a, 'i>(t: &'a Table<'i>, path: &[&str]) -> Option<&'a Table<'i>> {
    let mut cur = t;
    for key in path {
        match get(cur, key)? {
            DeValue::Table(next) => cur = next,
            _ => return None,
        }
    }
    Some(cur)
}

fn string<'a>(v: Option<&'a DeValue<'_>>) -> Option<&'a str> {
    match v? {
        DeValue::String(s) => Some(s),
        _ => None,
    }
}

fn array<'a, 'i>(v: Option<&'a DeValue<'i>>) -> &'a [Spanned<DeValue<'i>>] {
    match v {
        Some(DeValue::Array(a)) => a,
        _ => &[],
    }
}

/// Parses the parts of a `pyproject.toml` we need. `None` if it is not valid TOML.
pub fn parse_pyproject(text: &str) -> Option<PyprojectInfo> {
    let doc = DeTable::parse(text).ok()?;
    let doc = doc.get_ref();
    let mut info = PyprojectInfo::default();
    if let Some(project) = table(doc, &["project"]) {
        info.name = string(get(project, "name")).map(str::to_owned);
        info.requires_python = string(get(project, "requires-python")).and_then(requires_python_floor);
        let targets = |t: &Table<'_>, out: &mut Vec<String>| {
            for (_, v) in t.iter() {
                out.extend(string(Some(v.get_ref())).map(str::to_owned));
            }
        };
        for key in ["scripts", "gui-scripts"] {
            if let Some(DeValue::Table(t)) = get(project, key) {
                targets(t, &mut info.scripts);
            }
        }
        info.app_scripts = info.scripts.len() as u32;
        if let Some(DeValue::Table(groups)) = get(project, "entry-points") {
            for (_, g) in groups.iter() {
                if let DeValue::Table(t) = g.get_ref() {
                    targets(t, &mut info.scripts);
                }
            }
        }
    }
    let mut dirs: Vec<&str> = Vec::new();
    if let Some(st) = table(doc, &["tool", "setuptools"]) {
        if let Some(DeValue::Table(pd)) = get(st, "package-dir") {
            dirs.extend(string(get(pd, "")));
        }
        if let Some(find) = table(st, &["packages", "find"]) {
            for w in array(get(find, "where")) {
                dirs.extend(string(Some(w.get_ref())));
            }
        }
    }
    if let Some(poetry) = table(doc, &["tool", "poetry"]) {
        for p in array(get(poetry, "packages")) {
            if let DeValue::Table(p) = p.get_ref() {
                dirs.extend(string(get(p, "from")));
            }
        }
    }
    if let Some(wheel) = table(doc, &["tool", "hatch", "build", "targets", "wheel"]) {
        // `sources` (prefix rewrites) overrides the parent stripping `packages` implies: a
        // prefix rewritten to "" is a source directory (`"src" = ""`; `"" = ""` keeps paths).
        match get(wheel, "sources") {
            Some(DeValue::Table(t)) => {
                for (k, v) in t.iter() {
                    if string(Some(v.get_ref())) == Some("") {
                        dirs.push(k.get_ref().trim_end_matches('/'));
                    }
                }
            }
            Some(DeValue::Array(a)) => {
                for v in a {
                    dirs.extend(string(Some(v.get_ref())).map(|s| s.trim_end_matches('/')));
                }
            }
            _ => {
                for p in array(get(wheel, "packages")) {
                    if let Some(s) = string(Some(p.get_ref())) {
                        let s = s.trim_end_matches('/');
                        dirs.push(s.rsplit_once('/').map_or("", |(parent, _)| parent));
                    }
                }
            }
        }
    }
    if let Some(uv) = table(doc, &["tool", "uv", "build-backend"]) {
        dirs.extend(string(get(uv, "module-root")));
    } else if is_uv_build(doc) {
        dirs.push("src");
    }
    if let Some(pdm) = table(doc, &["tool", "pdm", "build"]) {
        dirs.extend(string(get(pdm, "package-dir")));
    }
    if let Some(maturin) = table(doc, &["tool", "maturin"]) {
        dirs.extend(string(get(maturin, "python-source")));
    }
    for d in dirs {
        let p = normalize(Path::new(d));
        if !p.is_absolute() && !info.source_dirs.contains(&p) {
            info.source_dirs.push(p);
        }
    }
    Some(info)
}

/// `[build-system] build-backend = "uv_build"` (whose default module root is `src`).
fn is_uv_build(doc: &Table<'_>) -> bool {
    table(doc, &["build-system"]).and_then(|b| string(get(b, "build-backend"))) == Some("uv_build")
}

/// Lowest `3.N` admitted by a PEP 440 specifier set such as `>=3.10,<4` or `~=3.11.2`.
/// Exclusion-only sets (`<3.13`, `!=3.9.*`) give `None`.
pub fn requires_python_floor(spec: &str) -> Option<PythonVersion> {
    let mut floor: Option<PythonVersion> = None;
    for clause in spec.split(',') {
        let c = clause.trim();
        // `>3.9` admits 3.9.1, so its floor is 3.9 too.
        let Some(ver) = ["===", ">=", "~=", "==", ">"].iter().find_map(|op| c.strip_prefix(op)) else { continue };
        let Some(v) = parse_version_prefix(ver.trim().as_bytes()) else { continue };
        floor = Some(floor.map_or(v, |f| f.max(v)));
    }
    floor
}

/// Reads `<root>/pyproject.toml`; `Ok(None)` if there is none.
pub fn read_pyproject(root: &Path) -> Result<Option<PyprojectInfo>, String> {
    let path = root.join(PYPROJECT_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => parse_pyproject(&text).map(Some).ok_or_else(|| "invalid TOML".to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// `root/src` holds a src layout: it is a directory, not itself a package (no `__init__.py`),
/// and contains at least one importable module or package: `name.py[i]`, or a directory `name`
/// with a Python file directly inside (`__init__.py`, or PEP 420 namespace content), where
/// `name` is an identifier. A monorepo `src/` of project directories (`src/my-lib/...`) is not
/// a src layout.
pub fn is_src_layout(src: &Path) -> bool {
    if src.join("__init__.py").exists() || src.join("__init__.pyi").exists() {
        return false;
    }
    let Ok(rd) = fs::read_dir(src) else { return false };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let b = name.as_bytes();
        let Ok(ft) = ent.file_type() else { continue };
        if ft.is_file() && crate::walk::is_python(b) {
            let stem = b.strip_suffix(b".pyi").or_else(|| b.strip_suffix(b".py")).unwrap_or(b);
            if is_identifier(stem) {
                return true;
            }
        } else if ft.is_dir() && b != b"__pycache__" && is_identifier(b) && dir_has_python(&ent.path()) {
            return true;
        }
    }
    false
}

fn dir_has_python(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| crate::walk::is_python(e.file_name().as_bytes()) && e.file_type().is_ok_and(|t| t.is_file()))
}
