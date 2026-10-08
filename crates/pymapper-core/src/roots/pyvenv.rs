//! Virtualenv and interpreter discovery: `pyvenv.cfg`, site-packages and the stdlib.
//!
//! No interpreter is ever run. The stdlib is found from the venv's `pyvenv.cfg`
//! (`base-prefix`, `home`, `base-executable`), the venv prefix itself (conda-style envs carry
//! their own stdlib), or a `python3.N` / `python3` executable on `PATH`; a candidate prefix
//! `P` qualifies when `P/lib/python3.N/os.py` exists (CPython's own landmark).

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::dist::{read_all, trim};
use super::{Diagnostic, DiagnosticKind};
use crate::config::PythonVersion;

/// The parts of a `pyvenv.cfg` we use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PyvenvCfg {
    /// `home`: the directory of the base interpreter (usually its `bin`).
    pub home: Option<PathBuf>,
    /// `version_info` (uv, virtualenv) or `version` (stdlib `venv`), as `3.N`.
    pub version: Option<PythonVersion>,
    /// `base-prefix` (virtualenv).
    pub base_prefix: Option<PathBuf>,
    /// `base-executable` (virtualenv) or `executable` (stdlib `venv`).
    pub base_executable: Option<PathBuf>,
    /// `include-system-site-packages = true`.
    pub include_system_site_packages: bool,
}

/// Parses `key = value` lines. Unknown keys and malformed lines are ignored.
pub fn parse_pyvenv_cfg(text: &[u8]) -> PyvenvCfg {
    let mut cfg = PyvenvCfg::default();
    let path = |v: &[u8]| (!v.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(v)));
    for line in text.split(|&b| b == b'\n') {
        let Some(eq) = line.iter().position(|&b| b == b'=') else { continue };
        let key = trim(&line[..eq]);
        let val = trim(&line[eq + 1..]);
        match key {
            b"home" => cfg.home = path(val),
            b"version_info" => cfg.version = parse_version_prefix(val).or(cfg.version),
            b"version" => cfg.version = cfg.version.or_else(|| parse_version_prefix(val)),
            b"base-prefix" => cfg.base_prefix = path(val),
            b"base-executable" => cfg.base_executable = path(val),
            b"executable" => cfg.base_executable = cfg.base_executable.take().or_else(|| path(val)),
            b"include-system-site-packages" => cfg.include_system_site_packages = val.eq_ignore_ascii_case(b"true"),
            _ => {}
        }
    }
    cfg
}

/// `3.N` from the start of `s` (`3.12.11`, `3.13t`, `3.9.0rc1`). `None` for Python 2 / junk.
pub fn parse_version_prefix(s: &[u8]) -> Option<PythonVersion> {
    let digits = |s: &[u8]| s.iter().take_while(|b| b.is_ascii_digit()).count();
    let n = digits(s);
    if n == 0 || s.get(n) != Some(&b'.') {
        return None;
    }
    let m = digits(&s[n + 1..]);
    if m == 0 || m > 3 || n > 3 {
        return None;
    }
    let major: u32 = std::str::from_utf8(&s[..n]).ok()?.parse().ok()?;
    let minor: u32 = std::str::from_utf8(&s[n + 1..n + 1 + m]).ok()?.parse().ok()?;
    if major != 3 || minor > 255 {
        return None;
    }
    Some(PythonVersion { major: 3, minor: minor as u8 })
}

/// How the virtualenv was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VenvSource {
    /// `venv` in the config / `--venv`.
    Configured,
    /// `$VIRTUAL_ENV`.
    Environment,
    /// `.venv` or `venv` under the project root.
    Project,
}

/// A virtualenv (or any interpreter prefix with a site-packages directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Venv {
    /// The venv directory (`sys.prefix`).
    pub prefix: PathBuf,
    pub source: VenvSource,
    /// `None` for a prefix without `pyvenv.cfg` (conda env, system prefix).
    pub cfg: Option<PyvenvCfg>,
    /// `lib/python3.N/site-packages` (usually exactly one).
    pub site_packages: Vec<PathBuf>,
    /// Version from `pyvenv.cfg`, else from the `python3.N` directory name.
    pub version: Option<PythonVersion>,
}

/// Opens the venv at `prefix`: it needs a `pyvenv.cfg` or a `lib/python3.N/site-packages`.
pub fn open_venv(prefix: &Path, source: VenvSource, version: Option<PythonVersion>, diags: &mut Vec<Diagnostic>) -> Option<Venv> {
    let cfg_path = prefix.join("pyvenv.cfg");
    let mut buf = Vec::new();
    let cfg = match read_all(&cfg_path, &mut buf) {
        Ok(()) => Some(parse_pyvenv_cfg(&buf)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            diags.push(Diagnostic { kind: DiagnosticKind::Venv, path: cfg_path, message: e.to_string() });
            None
        }
    };
    let version = version.or_else(|| cfg.as_ref().and_then(|c| c.version));
    let (site_packages, dir_version) = find_site_packages(prefix, version);
    if cfg.is_none() && site_packages.is_empty() {
        return None;
    }
    let version = version.or(dir_version);
    Some(Venv { prefix: prefix.to_path_buf(), source, cfg, site_packages, version })
}

/// Picks the virtualenv: `explicit` (config), else `$VIRTUAL_ENV`, else `.venv` / `venv` under
/// the project root. A configured or environment venv that cannot be opened is reported.
pub fn find_venv(
    project_root: &Path,
    explicit: Option<&Path>,
    virtual_env: Option<&Path>,
    version: Option<PythonVersion>,
    diags: &mut Vec<Diagnostic>,
) -> Option<Venv> {
    let not_venv = |path: &Path, what: &str| Diagnostic {
        kind: DiagnosticKind::Venv,
        path: path.to_path_buf(),
        message: format!("{what} is not a virtualenv (no pyvenv.cfg or lib/python3.N/site-packages)"),
    };
    if let Some(p) = explicit {
        let v = open_venv(p, VenvSource::Configured, version, diags);
        if v.is_none() {
            diags.push(not_venv(p, "configured `venv`"));
        }
        return v;
    }
    if let Some(p) = virtual_env.filter(|p| !p.as_os_str().is_empty()) {
        match open_venv(p, VenvSource::Environment, version, diags) {
            Some(v) => return Some(v),
            None => diags.push(not_venv(p, "$VIRTUAL_ENV")),
        }
    }
    for name in [".venv", "venv"] {
        let p = project_root.join(name);
        if p.is_dir()
            && let Some(v) = open_venv(&p, VenvSource::Project, version, diags)
        {
            return Some(v);
        }
    }
    None
}

/// `prefix/lib/python3.N[t]/site-packages` (for `version`, else the highest found) and
/// `prefix/Lib/site-packages` (Windows layout), plus the version read off the directory name.
pub fn find_site_packages(prefix: &Path, version: Option<PythonVersion>) -> (Vec<PathBuf>, Option<PythonVersion>) {
    let mut out = Vec::new();
    let found = lib_python_dirs(&prefix.join("lib"), version, |d| d.join("site-packages").is_dir());
    let mut ver = None;
    if let Some((dir, v)) = found {
        out.push(dir.join("site-packages"));
        ver = Some(v);
    }
    let win = prefix.join("Lib").join("site-packages");
    if win.is_dir() && !out.contains(&win) {
        out.push(win);
    }
    (out, ver)
}

/// The `lib/python3.N[t]` directory under `lib` accepted by `ok`: for `version` exactly, else
/// the highest version present.
fn lib_python_dirs(lib: &Path, version: Option<PythonVersion>, ok: impl Fn(&Path) -> bool) -> Option<(PathBuf, PythonVersion)> {
    if let Some(v) = version {
        for suffix in ["", "t"] {
            let d = lib.join(format!("python3.{}{suffix}", v.minor));
            if ok(&d) {
                return Some((d, v));
            }
        }
        return None;
    }
    let mut best: Option<(PathBuf, PythonVersion)> = None;
    let rd = fs::read_dir(lib).ok()?;
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(rest) = name.as_bytes().strip_prefix(b"python") else { continue };
        let Some(v) = parse_version_prefix(rest) else { continue };
        let tail = &rest[rest.iter().position(|&b| b == b'.').map_or(0, |i| i + 1)..];
        let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
        if !matches!(&tail[digits..], b"" | b"t") {
            continue;
        }
        let d = ent.path();
        if best.as_ref().is_none_or(|(bd, bv)| v > *bv || (v == *bv && d < *bd)) && ok(&d) {
            best = Some((d, v));
        }
    }
    best
}

/// The stdlib directory of `prefix` (`lib/python3.N[t]` containing `os.py`).
pub fn stdlib_in_prefix(prefix: &Path, version: Option<PythonVersion>) -> Option<(PathBuf, PythonVersion)> {
    lib_python_dirs(&prefix.join("lib"), version, |d| d.join("os.py").is_file())
}

/// Finds the stdlib for `version` (or any version when `None`): from the venv's base
/// interpreter, the venv prefix itself, then `PATH`. Returns the directory and its version.
pub fn find_stdlib(version: Option<PythonVersion>, venv: Option<&Venv>, path_env: Option<&OsStr>) -> Option<(PathBuf, PythonVersion)> {
    let mut prefixes: Vec<PathBuf> = Vec::new();
    let mut exes: Vec<PathBuf> = Vec::new();
    if let Some(v) = venv {
        if let Some(cfg) = &v.cfg {
            prefixes.extend(cfg.base_prefix.iter().cloned());
            if let Some(home) = &cfg.home {
                if home.file_name().is_some_and(|n| n == "bin")
                    && let Some(p) = home.parent()
                {
                    prefixes.push(p.to_path_buf());
                }
                prefixes.push(home.clone());
                if let Some(ver) = version {
                    exes.push(home.join(format!("python3.{}", ver.minor)));
                }
                exes.push(home.join("python3"));
                exes.push(home.join("python"));
            }
            exes.extend(cfg.base_executable.iter().cloned());
        }
        prefixes.push(v.prefix.clone());
    }
    for p in &prefixes {
        if let Some(found) = stdlib_in_prefix(p, version) {
            return Some(found);
        }
    }
    // Executables: resolve symlinks (Homebrew, framework builds), prefix = exe/../..
    if let Some(path_env) = path_env {
        for dir in path_env.as_bytes().split(|&b| b == b':').filter(|d| !d.is_empty()) {
            let dir = Path::new(OsStr::from_bytes(dir));
            if let Some(ver) = version {
                exes.push(dir.join(format!("python3.{}", ver.minor)));
            }
            exes.push(dir.join("python3"));
        }
    }
    for exe in &exes {
        let Ok(real) = fs::canonicalize(exe) else { continue };
        if let Some(prefix) = real.parent().and_then(Path::parent)
            && let Some(found) = stdlib_in_prefix(prefix, version)
        {
            return Some(found);
        }
    }
    None
}
