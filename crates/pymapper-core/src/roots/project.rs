//! First-party layout: project root (`pyproject.toml`), `src` layout and build-backend hints.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use toml::Spanned;
use toml::de::{DeTable, DeValue};

use super::pyvenv::parse_version_prefix;
use crate::config::{PYPROJECT_FILE, PythonVersion, normalize};

/// The nearest ancestor of `start` (inclusive) containing a `pyproject.toml`, else `start`.
pub fn find_project_root(start: &Path) -> PathBuf {
    start.ancestors().find(|d| d.join(PYPROJECT_FILE).is_file()).unwrap_or(start).to_path_buf()
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
        for p in array(get(wheel, "packages")) {
            if let Some(s) = string(Some(p.get_ref())) {
                let s = s.trim_end_matches('/');
                dirs.push(s.rsplit_once('/').map_or("", |(parent, _)| parent));
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
/// and contains at least one module or package (`__init__.py`, or PEP 420: any directory with a
/// Python file directly inside).
pub fn is_src_layout(src: &Path) -> bool {
    if src.join("__init__.py").exists() || src.join("__init__.pyi").exists() {
        return false;
    }
    let Ok(rd) = fs::read_dir(src) else { return false };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let b = name.as_bytes();
        if b.starts_with(b".") || b.ends_with(b".egg-info") || b == b"__pycache__" {
            continue;
        }
        let Ok(ft) = ent.file_type() else { continue };
        if ft.is_file() && crate::walk::is_python(b) {
            return true;
        }
        if ft.is_dir() && dir_has_python(&ent.path()) {
            return true;
        }
    }
    false
}

fn dir_has_python(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| crate::walk::is_python(e.file_name().as_bytes()) && e.file_type().is_ok_and(|t| t.is_file()))
}
