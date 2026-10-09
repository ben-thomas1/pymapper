//! Tests for import roots, with synthetic project / venv / interpreter layouts in temp dirs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::dist::{file_url_path, for_each_record_path, json_bool, json_str, metadata_headers, split_dist_dir};
use super::pth::{PthItem, hook_mappings, parse_pth};
use super::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-roots-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        // Canonical, so paths compare equal to what discovery derives (macOS /var -> /private/var).
        TempDir(fs::canonicalize(&dir).expect("canonicalize temp dir"))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Creates the file `rel` (and parent directories) with `contents`.
    fn file(&self, rel: &str, contents: &str) -> &TempDir {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        self
    }

    fn dir(&self, rel: &str) -> &TempDir {
        fs::create_dir_all(self.0.join(rel)).expect("mkdir");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn s(b: &[u8]) -> &str {
    std::str::from_utf8(b).expect("utf-8")
}

fn v(minor: u8) -> PythonVersion {
    PythonVersion { major: 3, minor }
}

// ---------------------------------------------------------------------------------------------
// Pure parsers

#[test]
fn pyvenv_cfg_variants() {
    let uv = b"home = /opt/py/bin\nimplementation = CPython\nuv = 0.11.16\nversion_info = 3.12.11\ninclude-system-site-packages = false\nprompt = x\n";
    let c = parse_pyvenv_cfg(uv);
    assert_eq!(c.home.as_deref(), Some(Path::new("/opt/py/bin")));
    assert_eq!(c.version, Some(v(12)));
    assert!(!c.include_system_site_packages);

    let stdlib_venv = b"home = /usr/bin\ninclude-system-site-packages = TRUE\nversion = 3.11.4\nexecutable = /usr/bin/python3.11\ncommand = /usr/bin/python3 -m venv /x\n";
    let c = parse_pyvenv_cfg(stdlib_venv);
    assert_eq!(c.version, Some(v(11)));
    assert!(c.include_system_site_packages);
    assert_eq!(c.base_executable.as_deref(), Some(Path::new("/usr/bin/python3.11")));

    // virtualenv: version_info wins over version, base-executable over executable.
    let ve = b"version = 3.9.1\nversion_info = 3.10.2.final.0\nbase-prefix = /b\nbase-executable = /b/bin/python3\nexecutable = /other\r\n";
    let c = parse_pyvenv_cfg(ve);
    assert_eq!(c.version, Some(v(10)));
    assert_eq!(c.base_prefix.as_deref(), Some(Path::new("/b")));
    assert_eq!(c.base_executable.as_deref(), Some(Path::new("/b/bin/python3")));

    let junk = b"garbage\n= \nhome =\nversion = 2.7.18\n";
    assert_eq!(parse_pyvenv_cfg(junk), PyvenvCfg::default());
}

#[test]
fn version_prefix() {
    assert_eq!(parse_version_prefix(b"3.13.5"), Some(v(13)));
    assert_eq!(parse_version_prefix(b"3.14t"), Some(v(14)));
    assert_eq!(parse_version_prefix(b"3.9.0rc1"), Some(v(9)));
    assert_eq!(parse_version_prefix(b"2.7"), None);
    assert_eq!(parse_version_prefix(b"3"), None);
    assert_eq!(parse_version_prefix(b"3."), None);
    assert_eq!(parse_version_prefix(b"x3.1"), None);
    assert_eq!(parse_version_prefix(b"3.1234"), None);
}

#[test]
fn requires_python() {
    assert_eq!(requires_python_floor(">=3.10"), Some(v(10)));
    assert_eq!(requires_python_floor(">=3.9, <4"), Some(v(9)));
    assert_eq!(requires_python_floor("~=3.11.2"), Some(v(11)));
    assert_eq!(requires_python_floor("==3.12.*"), Some(v(12)));
    assert_eq!(requires_python_floor(">3.8"), Some(v(8)));
    assert_eq!(requires_python_floor(">=3.8,>=3.10"), Some(v(10)));
    assert_eq!(requires_python_floor("<3.13"), None);
    assert_eq!(requires_python_floor("!=3.9.*"), None);
    assert_eq!(requires_python_floor(""), None);
}

#[test]
fn pyproject_hints() {
    let text = r#"
[project]
name = "My.Project"
requires-python = ">=3.11"

[tool.setuptools.package-dir]
"" = "lib"

[tool.setuptools.packages.find]
where = ["src", "lib"]

[tool.poetry]
packages = [{ include = "foo", from = "poetry_src" }, { include = "bar" }]

[tool.hatch.build.targets.wheel]
packages = ["hatch_src/pkg", "toplevel"]

[tool.uv.build-backend]
module-root = "uvroot"

[tool.pdm.build]
package-dir = "pdm_src"

[tool.maturin]
python-source = "python"
"#;
    let info = parse_pyproject(text).expect("valid");
    assert_eq!(info.name.as_deref(), Some("My.Project"));
    assert_eq!(info.requires_python, Some(v(11)));
    let dirs: Vec<_> = info.source_dirs.iter().map(|p| p.to_str().expect("utf-8")).collect();
    assert_eq!(dirs, ["lib", "src", "poetry_src", "hatch_src", ".", "uvroot", "pdm_src", "python"]);

    let uv_default = "[build-system]\nbuild-backend = \"uv_build\"\n";
    assert_eq!(parse_pyproject(uv_default).expect("valid").source_dirs, [PathBuf::from("src")]);
    assert!(parse_pyproject("[project\n").is_none());
    assert_eq!(parse_pyproject("").expect("valid"), PyprojectInfo::default());
    // Absolute paths are ignored, wrong types too.
    let odd = "[tool.pdm.build]\npackage-dir = \"/abs\"\n[tool.maturin]\npython-source = 3\n";
    assert!(parse_pyproject(odd).expect("valid").source_dirs.is_empty());
}

#[test]
fn pyproject_entry_point_scripts() {
    let text = r#"
[project]
name = "app"

[project.scripts]
app = "app.cli:main"
tool = "app.tools:run [extra]"

[project.gui-scripts]
gui = "app.gui:start"

[project.entry-points."pytest11"]
plug = "app.plugin"

[project.entry-points.weird]
bad = 3
"#;
    let info = parse_pyproject(text).expect("valid");
    assert_eq!(info.scripts, ["app.cli:main", "app.tools:run [extra]", "app.gui:start", "app.plugin"]);
    assert!(parse_pyproject("[project]\nscripts = 1\n").expect("valid").scripts.is_empty());
}

#[test]
fn metadata_header_parsing() {
    let m = b"Metadata-Version: 2.4\r\nName: PyYAML\r\nSummary: x\r\nVersion: 6.0.2\r\n\r\nName: body\n";
    let h = metadata_headers(m);
    assert_eq!(h.name, Some(&b"PyYAML"[..]));
    assert_eq!(h.version, Some(&b"6.0.2"[..]));

    // Continuation lines are skipped, lower-case keys accepted, body ignored.
    let m = b"Metadata-Version: 1.1\nDescription: long\n        Name: fake\nname:  foo \n\nVersion: 9\n";
    let h = metadata_headers(m);
    assert_eq!(h.name, Some(&b"foo"[..]));
    assert_eq!(h.version, None);
    assert!(h.done);

    let h = metadata_headers(b"Name: x\nSummary: truncated");
    assert!(!h.done);
}

#[test]
fn dist_dir_names() {
    assert_eq!(split_dist_dir(b"PyYAML-6.0.2"), (&b"PyYAML"[..], &b"6.0.2"[..]));
    assert_eq!(split_dist_dir(b"zope.interface-7.1"), (&b"zope.interface"[..], &b"7.1"[..]));
    assert_eq!(split_dist_dir(b"foo-1.0-py3.8"), (&b"foo"[..], &b"1.0"[..]));
    assert_eq!(split_dist_dir(b"noversion"), (&b"noversion"[..], &b""[..]));
}

#[test]
fn name_normalisation() {
    let mut out = Vec::new();
    for (input, want) in [
        ("PyYAML", "pyyaml"),
        ("zope.interface", "zope-interface"),
        ("Foo__Bar-._baz", "foo-bar-baz"),
        ("_leading", "leading"),
        ("trailing-", "trailing"),
        ("a", "a"),
    ] {
        normalize_name(input.as_bytes(), &mut out);
        assert_eq!(s(&out), want, "{input}");
    }
}

#[test]
fn record_csv() {
    let rec = b"foo/__init__.py,sha256=abc,12\r\n\"we,ird/\"\"q\"\".py\",sha256=x,1\nfoo-1.0.dist-info/RECORD,,\n\nplain\n";
    let mut got = Vec::new();
    let mut scratch = Vec::new();
    for_each_record_path(rec, &mut scratch, |p| got.push(s(p).to_owned()));
    assert_eq!(got, ["foo/__init__.py", "we,ird/\"q\".py", "foo-1.0.dist-info/RECORD", "plain"]);
}

#[test]
fn json_helpers() {
    let j = br#"{"url": "file:///home/me/my%20proj/", "dir_info": {"editable" :  true}}"#;
    assert_eq!(json_bool(j, b"editable"), Some(true));
    let mut scratch = Vec::new();
    let url = json_str(j, b"url", &mut scratch).expect("url").to_vec();
    assert_eq!(s(&url), "file:///home/me/my%20proj/");
    assert_eq!(s(&file_url_path(&url).expect("file url")), "/home/me/my proj");
    assert_eq!(json_bool(br#"{"editable": false}"#, b"editable"), Some(false));
    assert_eq!(json_bool(br#"{"noneditable": true}"#, b"editable"), None);
    assert_eq!(json_bool(br#"{"x": "editable", "editable": true}"#, b"editable"), Some(true));
    assert!(json_str(br#"{"url": "unterminated}"#, b"url", &mut scratch).is_none());
    let esc = br#"{"url": "file:///a\/b\"c"}"#;
    assert_eq!(s(json_str(esc, b"url", &mut scratch).expect("url")), "file:///a/b\"c");
    assert_eq!(file_url_path(b"https://x/y"), None);
    assert_eq!(s(&file_url_path(b"file://localhost/srv/x").expect("local")), "/srv/x");
    assert_eq!(s(&file_url_path(b"file:///a%2").expect("bad escape kept")), "/a%2");
}

#[test]
fn identifiers() {
    for ok in ["a", "_", "abc_1", "Ünïcode", "_x9"] {
        assert!(is_identifier(ok.as_bytes()), "{ok}");
    }
    for bad in ["", "1a", "a-b", "a.b", "a b", "foo-stubs"] {
        assert!(!is_identifier(bad.as_bytes()), "{bad}");
    }
}

// ---------------------------------------------------------------------------------------------
// .pth files and editable hooks

const SETUPTOOLS_FINDER: &str = r#"from __future__ import annotations
import sys
from importlib.machinery import ModuleSpec, PathFinder

MAPPING: dict[str, str] = {'mypkg': '/src/proj/mypkg', "single": "/src/proj/single.py", 'bad-name': '/x', 'rel': 'not/abs'}
NAMESPACES: dict[str, list[str]] = {'ns': ['/src/a/ns', '/src/b/ns'], 'ns2': []}
PATH_PLACEHOLDER = '__editable__.mypkg-0.1.0.finder' + ".__path_hook__"

class _EditableFinder:
    @classmethod
    def find_spec(cls, fullname, path=None, target=None):
        if fullname in MAPPING:
            pkg_path = MAPPING[fullname]
"#;

#[test]
fn hook_mapping_scan() {
    let got: Vec<(String, String)> =
        hook_mappings(SETUPTOOLS_FINDER.as_bytes()).into_iter().map(|(a, b)| (s(&a).to_owned(), s(&b).to_owned())).collect();
    let want = [("mypkg", "/src/proj/mypkg"), ("single", "/src/proj/single.py"), ("ns", "/src/a/ns"), ("ns", "/src/b/ns")];
    assert_eq!(got, want.map(|(a, b)| (a.to_owned(), b.to_owned())));

    // Older setuptools: no annotation; escapes in repr'd strings.
    let old = b"MAPPING = {\n    'p': '/a\\\\b/p',  # comment\n}\nMAPPING_X = {'q': '/q'}\n";
    let got = hook_mappings(old);
    assert_eq!(got, [(b"p".to_vec(), b"/a\\b/p".to_vec())]);

    // `editables` redirector.
    let ed = b"from editables.redirector import RedirectingFinder as F\nF.install()\nF.map_module('pkg', '/src/pkg/__init__.py')\nF.map_module(\"a.b\", r'/src/a/b.py')\ndef map_module(name, path): pass\n";
    let got = hook_mappings(ed);
    assert_eq!(got, [(b"pkg".to_vec(), b"/src/pkg/__init__.py".to_vec()), (b"a.b".to_vec(), b"/src/a/b.py".to_vec())]);

    // Garbage never panics.
    for junk in [&b"MAPPING = {'a': "[..], b"MAPPING = {'a' 'b'}", b"MAPPING: = ", b"map_module('x'", b"MAPPING == {'a': '/b'}", b"MAPPING = {'a': ['/b', }"] {
        let _ = hook_mappings(junk);
    }
    assert!(hook_mappings(b"MAPPING == {'a': '/b'}").is_empty());
}

#[test]
fn pth_lines() {
    let t = TempDir::new("pth");
    t.dir("site").dir("src/one").dir("site/rel");
    let site = t.p("site");
    let text = format!(
        "\u{feff}# comment\n\n{one}\nrel\n../site/rel\n/does/not/exist\nimport sys; sys.path.append('x')\n   \nimport\tnothing\n{one}   \n",
        one = t.p("src/one").display()
    );
    let mut out = Vec::new();
    parse_pth(&site, b"a.pth", text.as_bytes(), &mut out);
    let paths: Vec<PathBuf> = out
        .iter()
        .map(|i| match i {
            PthItem::Path { path, pth } => {
                assert_eq!(&**pth, b"a.pth");
                path.clone()
            }
            PthItem::Mapped { .. } => panic!("unexpected mapping"),
        })
        .collect();
    // Duplicates are kept here; the root builder deduplicates.
    assert_eq!(paths, [t.p("src/one"), t.p("site/rel"), t.p("site/rel"), t.p("src/one")]);
}

#[test]
fn pth_hook_files() {
    let t = TempDir::new("pthhook");
    t.file("site/__editable__.mypkg-0.1.0.pth", "import __editable___mypkg_0_1_0_finder; __editable___mypkg_0_1_0_finder.install()\n")
        .file("site/__editable___mypkg_0_1_0_finder.py", SETUPTOOLS_FINDER)
        .file("site/_mypkg_editable.pth", "import _mypkg_editable\n")
        .file("site/_mypkg_editable.py", "F.map_module('other', '/src/other/__init__.py')\n")
        .file("site/.hidden.pth", "/src\n")
        .file("site/virtualenv.pth", "import _virtualenv\n");
    let mut out = Vec::new();
    let mut diags = Vec::new();
    pth::read_site_pth(&t.p("site"), &mut out, &mut diags);
    assert!(diags.is_empty(), "{diags:?}");
    let got: Vec<(String, String, String)> = out
        .iter()
        .map(|i| match i {
            PthItem::Mapped { name, path, pth } => (s(name).to_owned(), path.display().to_string(), s(pth).to_owned()),
            PthItem::Path { .. } => panic!("unexpected path"),
        })
        .collect();
    let e = "__editable__.mypkg-0.1.0.pth";
    let want = [
        ("mypkg", "/src/proj/mypkg", e),
        ("single", "/src/proj/single.py", e),
        ("ns", "/src/a/ns", e),
        ("ns", "/src/b/ns", e),
        ("other", "/src/other", "_mypkg_editable.pth"),
    ];
    assert_eq!(got, want.map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned())));
}

// ---------------------------------------------------------------------------------------------
// Module map (PEP 420 / PEP 561 / stub preference)

/// Builds a map from (root, rel) pairs and returns (name, status) per file.
fn statuses(files: &[(RootId, &str)]) -> (ModuleMap, Vec<(Option<String>, FileStatus)>) {
    let mut b = ModuleMapBuilder::new();
    for &(r, rel) in files {
        b.add(r, b"", rel.as_bytes(), false);
    }
    let m = b.finish();
    let out = (0..files.len() as u32).map(|f| (m.file_module_name(f).map(|n| s(n).to_owned()), m.file_status(f))).collect();
    (m, out)
}

fn st(name: &str, status: FileStatus) -> (Option<String>, FileStatus) {
    (Some(name.to_owned()), status)
}

#[test]
fn module_names_and_kinds() {
    use FileStatus::*;
    let (m, got) = statuses(&[
        (0, "pkg/__init__.py"),
        (0, "pkg/mod.py"),
        (0, "pkg/sub/deep.py"),
        (0, "top.py"),
        (0, "my-script.py"),
        (0, "__init__.py"),
        (0, "pkg/data.txt"),
        (0, "a.b/c.py"),
        (0, "1bad/x.py"),
    ]);
    assert_eq!(
        got,
        [
            st("pkg", Active),
            st("pkg.mod", Active),
            st("pkg.sub.deep", Active),
            st("top", Active),
            (None, NotImportable),
            (None, NotImportable),
            (None, NotImportable),
            (None, NotImportable),
            (None, NotImportable),
        ]
    );
    let k = |n: &str| m.lookup(n.as_bytes()).and_then(|s| m.kind(s));
    assert_eq!(k("pkg"), Some(ModuleKind::Package));
    assert_eq!(k("pkg.mod"), Some(ModuleKind::Module));
    assert_eq!(k("pkg.sub"), Some(ModuleKind::Namespace));
    assert_eq!(k("missing"), None);
    let sub = m.lookup(b"pkg.sub").expect("pkg.sub");
    assert_eq!(m.parent(sub), m.lookup(b"pkg"));
    assert_eq!(m.search_roots(sub), &[0]);
    assert_eq!(m.file(m.lookup(b"pkg.mod").expect("mod")), Some(1));
    assert_eq!(m.root(m.lookup(b"top").expect("top")), Some(0));
    assert!(m.heap_bytes() > 0);
}

#[test]
fn root_precedence() {
    use FileStatus::*;
    let (m, got) = statuses(&[
        // Earlier root's module beats a later root's package and module.
        (1, "dup/__init__.py"),
        (0, "dup.py"),
        (2, "dup.py"),
        // Same root: package beats module.
        (0, "both/__init__.py"),
        (0, "both.py"),
        // Namespace portion in root 0 loses to a regular package in root 1.
        (0, "nsreg/a.py"),
        (1, "nsreg/__init__.py"),
        (1, "nsreg/b.py"),
        // Pure namespace across roots: portions merge.
        (0, "ns/x.py"),
        (2, "ns/y.py"),
        (1, "ns/x.py"),
        // Files below a module are unreachable.
        (0, "modfirst.py"),
        (1, "modfirst/inner.py"),
    ]);
    assert_eq!(
        got,
        [
            st("dup", Shadowed),
            st("dup", Active),
            st("dup", Shadowed),
            st("both", Active),
            st("both", Shadowed),
            st("nsreg.a", Shadowed),
            st("nsreg", Active),
            st("nsreg.b", Active),
            st("ns.x", Active),
            st("ns.y", Active),
            st("ns.x", Shadowed),
            st("modfirst", Active),
            st("modfirst.inner", Shadowed),
        ]
    );
    let ns = m.lookup(b"ns").expect("ns");
    assert_eq!(m.kind(ns), Some(ModuleKind::Namespace));
    assert_eq!(m.search_roots(ns), &[0, 1, 2]);
    assert_eq!(m.file(ns), None);
    assert_eq!(m.kind(m.lookup(b"modfirst.inner").expect("inner")), None);
}

#[test]
fn stubs_preferred() {
    use FileStatus::*;
    let (m, got) = statuses(&[
        (0, "lib/__init__.py"),
        (0, "lib/__init__.pyi"),
        (0, "lib/core.py"),
        (0, "lib/core.pyi"),
        (0, "lib/only_py.py"),
        // PEP 561 stub-only package next to the runtime package.
        (0, "ext-stubs/__init__.pyi"),
        (0, "ext/__init__.py"),
        (0, "ext/fast.py"),
        (0, "ext-stubs/fast.pyi"),
        // A stub in a later root does not shadow an earlier root's module.
        (0, "early.py"),
        (1, "early.pyi"),
        // Two stubs for one module: the first claims it.
        (1, "dual-stubs/__init__.pyi"),
        (1, "dual/__init__.pyi"),
    ]);
    assert_eq!(
        got,
        [
            st("lib", StubShadowed),
            st("lib", Active),
            st("lib.core", StubShadowed),
            st("lib.core", Active),
            st("lib.only_py", Active),
            st("ext", Active),
            st("ext", StubShadowed),
            st("ext.fast", StubShadowed),
            st("ext.fast", Active),
            st("early", Active),
            st("early", Shadowed),
            st("dual", Active),
            st("dual", Shadowed),
        ]
    );
    let core = m.lookup(b"lib.core").expect("core");
    assert_eq!(m.file(core), Some(3));
    assert_eq!(m.implementation(core), Some(2));
    let only = m.lookup(b"lib.only_py").expect("only");
    assert_eq!(m.implementation(only), Some(4));
    assert_eq!(m.implementation(m.lookup(b"dual").expect("dual")), None);
}

#[test]
fn prefixed_and_single_file_roots() {
    let mut b = ModuleMapBuilder::new();
    let f0 = b.add(0, b"mypkg", b"__init__.py", false);
    let f1 = b.add(0, b"mypkg", b"sub/m.py", false);
    let f2 = b.add(1, b"single", b"single.py", true);
    let f3 = b.add(2, b"a.b", b"c.pyi", false);
    let f4 = b.add_outside();
    let m = b.finish();
    assert_eq!(m.file_module_name(f0), Some(&b"mypkg"[..]));
    assert_eq!(m.file_module_name(f1), Some(&b"mypkg.sub.m"[..]));
    assert_eq!(m.file_module_name(f2), Some(&b"single"[..]));
    assert_eq!(m.file_module_name(f3), Some(&b"a.b.c"[..]));
    assert_eq!(m.file_status(f4), FileStatus::Outside);
    assert_eq!(m.file_root(f4), None);
    assert_eq!(m.file_root(f2), Some(1));
    for f in [f0, f1, f2, f3] {
        assert_eq!(m.file_status(f), FileStatus::Active, "{f}");
    }
}

// ---------------------------------------------------------------------------------------------
// Discovery on synthetic layouts

const SITE: &str = "proj/.venv/lib/python3.12/site-packages";

/// Writes `<site>/<name>-<version>.dist-info` with METADATA, a RECORD listing `files` (each
/// file is also created, with empty contents unless it is a `.pth`, whose content is given
/// after `=`), and optional top_level.txt / direct_url.json.
fn dist(t: &TempDir, site: &str, name: &str, version: &str, files: &[&str], top_level: Option<&str>, direct_url: Option<&str>) {
    let info = format!("{site}/{}-{version}.dist-info", name.replace('-', "_"));
    t.file(&format!("{info}/METADATA"), &format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n\nbody\n"));
    let mut record = String::new();
    for f in files {
        let (path, contents) = f.split_once('=').unwrap_or((f, ""));
        t.file(&format!("{site}/{path}"), contents);
        record.push_str(&format!("{path},sha256=x,1\n"));
    }
    let info_name = format!("{}-{version}.dist-info", name.replace('-', "_"));
    record.push_str(&format!("{info_name}/METADATA,,\n{info_name}/RECORD,,\n../../../bin/tool,,\n"));
    t.file(&format!("{info}/RECORD"), &record);
    if let Some(tl) = top_level {
        t.file(&format!("{info}/top_level.txt"), tl);
    }
    if let Some(du) = direct_url {
        t.file(&format!("{info}/direct_url.json"), du);
    }
}

/// The full fixture: project with src layout and a workspace member, venv with assorted
/// distributions and `.pth` files, a base interpreter with a stdlib, external sources.
fn fixture() -> TempDir {
    let t = TempDir::new("disc");
    let root = t.path().display().to_string();
    t.file("proj/pyproject.toml", "[project]\nname = \"Proj\"\nrequires-python = \">=3.11\"\n")
        .file("proj/src/proj/__init__.py", "")
        .file("proj/src/proj/core.py", "")
        .file("proj/tests/test_core.py", "")
        .file("proj/libs/member/member/__init__.py", "")
        .file("proj/.venv/pyvenv.cfg", &format!("home = {root}/python/bin\nversion_info = 3.12.3\n"))
        .file("python/lib/python3.12/os.py", "")
        .file("python/lib/python3.12/json/__init__.py", "")
        .file("python/lib/python3.12/site-packages/sys_pkg.py", "")
        .dir("python/bin")
        .file("external/src/extpkg/__init__.py", "")
        .file("external/hooked/__init__.py", "")
        .file(&format!("{SITE}/stray.py"), "")
        .file(&format!("{SITE}/norecord/__init__.py"), "")
        .file(&format!("{SITE}/norecord-1.0.dist-info/METADATA"), "Name: norecord\nVersion: 1.0\n")
        .file(&format!("{SITE}/ext.pth"), &format!("{root}/external/src\n"));
    dist(&t, SITE, "PyYAML", "6.0.2", &["yaml/__init__.py", "yaml/_yaml.cpython-312-darwin.so", "_yaml/__init__.py"], Some("_yaml\nyaml\n"), None);
    dist(&t, SITE, "protobuf", "5.0", &["google/protobuf/__init__.py", "google/protobuf/msg.py"], None, None);
    dist(&t, SITE, "googleapis-common-protos", "1.0", &["google/api/x.py", "google/api/x.pyi"], None, None);
    dist(&t, SITE, "requests", "2.32.0", &["requests/__init__.py", "requests/api.py"], None, None);
    dist(&t, SITE, "types-requests", "2.32.0.1", &["requests-stubs/__init__.pyi", "requests-stubs/api.pyi"], None, None);
    let member_pth = format!("_member.pth={root}/proj/libs/member\n");
    let editable = format!("{{\"url\": \"file://{root}/proj/libs/member\", \"dir_info\": {{\"editable\": true}}}}");
    dist(&t, SITE, "member", "0.1", &[&member_pth], None, Some(&editable));
    let proj_pth = format!("_proj.pth={root}/proj/src\n");
    dist(&t, SITE, "proj", "0.1", &[&proj_pth], None, Some("{\"url\": \"file:///x\", \"dir_info\": {\"editable\": true}}"));
    let hook_pth = "__editable__.hooked-1.0.pth=import __editable___hooked_1_0_finder; __editable___hooked_1_0_finder.install()\n";
    let finder = format!("__editable___hooked_1_0_finder.py=MAPPING = {{'hooked': '{root}/external/hooked'}}\n");
    dist(&t, SITE, "hooked", "1.0", &[hook_pth, &finder], None, None);
    t
}

fn opts() -> RootOptions {
    RootOptions { path_env: Some(Default::default()), ..RootOptions::default() }
}

fn rel_roots(t: &TempDir, r: &Roots) -> Vec<(String, RootKind, Origin, bool)> {
    r.roots
        .iter()
        .map(|x| {
            let p = x.path.strip_prefix(t.path()).map_or_else(|_| x.path.display().to_string(), |p| p.display().to_string());
            (p, x.kind, x.origin, x.analyse)
        })
        .collect()
}

#[test]
fn discover_full_layout() {
    let t = fixture();
    let r = discover(&t.p("proj"), &RootOptions { include_deps: true, include_stdlib: true, ..opts() });
    use Origin::*;
    use RootKind::*;
    let want = [
        ("proj/src", Src, FirstParty, true),
        ("proj", Project, FirstParty, true),
        ("python/lib/python3.12", RootKind::Stdlib, Origin::Stdlib, true),
        (SITE, SitePackages, ThirdParty, true),
        ("proj/libs/member", Pth, FirstParty, true),
        ("external/src", Pth, ThirdParty, true),
        ("external/hooked", EditableHook, ThirdParty, true),
    ];
    assert_eq!(rel_roots(&t, &r), want.map(|(p, k, o, a)| (p.to_owned(), k, o, a)));
    assert_eq!(r.python_version, Some(v(12)));
    assert_eq!(r.version_source, Some(VersionSource::Venv));
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("python/lib/python3.12")));
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!(venv.source, VenvSource::Project);
    assert_eq!(venv.site_packages, [t.p(SITE)]);
    assert_eq!(&*r.roots[6].prefix, b"hooked");

    // Diagnostics: only the distribution without a RECORD.
    assert_eq!(r.diagnostics.len(), 1, "{:?}", r.diagnostics);
    assert_eq!(r.diagnostics[0].kind, DiagnosticKind::Record);

    // Distributions, sorted by metadata dir name.
    let d = &r.dists;
    let names: Vec<&str> = d.ids().map(|i| s(d.name(i))).collect();
    assert_eq!(names, ["PyYAML", "googleapis-common-protos", "hooked", "member", "norecord", "proj", "protobuf", "requests", "types-requests"]);
    let yaml = d.by_name(b"pyyaml").expect("pyyaml");
    assert_eq!(d.by_name(b"PyYAML"), Some(yaml));
    assert_eq!(s(d.version(yaml)), "6.0.2");
    assert_eq!(s(d.normalized_name(d.by_name(b"googleapis_common.protos").expect("g"))), "googleapis-common-protos");
    assert_eq!(d.files(yaml).map(s).collect::<Vec<_>>(), ["yaml/__init__.py", "_yaml/__init__.py"]);
    assert_eq!(d.top_level(yaml).map(s).collect::<Vec<_>>(), ["_yaml", "yaml"]);
    assert!(d.has_record(yaml) && d.has_top_level_txt(yaml) && !d.is_editable(yaml) && !d.is_egg_info(yaml));
    assert_ne!(d.record_hash(yaml), 0);
    assert_eq!(d.info_path(yaml), t.p(&format!("{SITE}/PyYAML-6.0.2.dist-info")));
    assert_eq!(d.providers(b"yaml"), &[yaml]);
    let google: Vec<&str> = d.providers(b"google").iter().map(|&i| s(d.name(i))).collect();
    assert_eq!(google, ["googleapis-common-protos", "protobuf"]);
    let req: Vec<&str> = d.providers(b"requests").iter().map(|&i| s(d.name(i))).collect();
    assert_eq!(req, ["requests", "types-requests"]);
    assert!(d.providers(b"nope").is_empty());
    let member = d.by_name(b"member").expect("member");
    assert!(d.is_editable(member));
    assert_eq!(d.editable_source(member), Some(&*t.p("proj/libs/member")));
    let hooked = d.by_name(b"hooked").expect("hooked");
    assert_eq!(d.top_level(hooked).map(s).collect::<Vec<_>>(), ["hooked"]);
    assert!(d.files(hooked).next().is_none(), "editable finder files are not code");
    let norecord = d.by_name(b"norecord").expect("norecord");
    assert!(!d.has_record(norecord));
    assert_eq!(d.owner(0, b"google/api/x.pyi"), d.by_name(b"googleapis-common-protos"));
    assert_eq!(d.owner(0, b"stray.py"), None);
    assert_eq!(d.owner(7, b"stray.py"), None);
    assert!(d.file_count() >= 12);
    assert!(d.heap_bytes() > 0);

    // Roots carry their distributions.
    let proj = d.by_name(b"proj");
    assert!(proj.is_some());
    assert_eq!(r.roots[0].dist, proj, "src root is the editable project's");
    assert_eq!(r.roots[1].dist, proj, "first-party roots belong to the project");
    assert_eq!(r.roots[4].dist, Some(member));
    assert_eq!(r.roots[6].dist, Some(hooked));
    assert_eq!(r.site_root(0), Some(3));
}

#[test]
fn classify_files() {
    let t = fixture();
    let r = discover(&t.p("proj"), &RootOptions { include_deps: true, ..opts() });
    let c = |rel: &str| {
        let p = t.p(rel);
        r.classify(&p).map(|l| (l.root, l.origin, s(l.rel).to_owned(), l.dist.map(|d| s(r.dists.name(d)).to_owned())))
    };
    let some = |root, origin, rel: &str, dist: Option<&str>| Some((root, origin, rel.to_owned(), dist.map(str::to_owned)));
    use Origin::*;
    assert_eq!(c("proj/src/proj/core.py"), some(0, FirstParty, "proj/core.py", Some("proj")));
    assert_eq!(c("proj/tests/test_core.py"), some(1, FirstParty, "tests/test_core.py", Some("proj")));
    assert_eq!(c("proj/libs/member/member/__init__.py"), some(4, FirstParty, "member/__init__.py", Some("member")));
    assert_eq!(c(&format!("{SITE}/yaml/__init__.py")), some(3, ThirdParty, "yaml/__init__.py", Some("PyYAML")));
    assert_eq!(c(&format!("{SITE}/stray.py")), some(3, ThirdParty, "stray.py", None));
    assert_eq!(c("python/lib/python3.12/json/__init__.py"), some(2, Stdlib, "json/__init__.py", None));
    assert_eq!(c("external/hooked/__init__.py"), some(6, ThirdParty, "__init__.py", Some("hooked")));
    assert_eq!(c("external/srcfoo/x.py"), None, "prefix match must stop at a component boundary");
    assert_eq!(c("elsewhere/x.py"), None);
    assert_eq!(c("proj/src").map(|l| l.0), Some(1), "a directory path is classified by its parent root");
}

#[test]
fn module_map_over_layout() {
    let t = fixture();
    let r = discover(&t.p("proj"), &RootOptions { include_deps: true, include_stdlib: true, ..opts() });
    let walk = crate::walk::walk(&[t.path()], &WalkOptions::everything());
    let m = r.module_map(&walk.files);
    let mut got = Vec::new();
    for (i, p) in walk.files.iter().enumerate() {
        let rel = p.strip_prefix(t.path()).expect("under temp").display().to_string();
        let name = m.file_module_name(i as u32).map(|n| s(n).to_owned()).unwrap_or_default();
        got.push(format!("{rel} {name} {:?}", m.file_status(i as u32)));
    }
    let want = [
        "external/hooked/__init__.py hooked Active",
        "external/src/extpkg/__init__.py extpkg Active",
        "proj/.venv/lib/python3.12/site-packages/__editable___hooked_1_0_finder.py __editable___hooked_1_0_finder Active",
        "proj/.venv/lib/python3.12/site-packages/_yaml/__init__.py _yaml Active",
        "proj/.venv/lib/python3.12/site-packages/google/api/x.py google.api.x StubShadowed",
        "proj/.venv/lib/python3.12/site-packages/google/api/x.pyi google.api.x Active",
        "proj/.venv/lib/python3.12/site-packages/google/protobuf/__init__.py google.protobuf Active",
        "proj/.venv/lib/python3.12/site-packages/google/protobuf/msg.py google.protobuf.msg Active",
        "proj/.venv/lib/python3.12/site-packages/norecord/__init__.py norecord Active",
        "proj/.venv/lib/python3.12/site-packages/requests/__init__.py requests StubShadowed",
        "proj/.venv/lib/python3.12/site-packages/requests/api.py requests.api StubShadowed",
        "proj/.venv/lib/python3.12/site-packages/requests-stubs/__init__.pyi requests Active",
        "proj/.venv/lib/python3.12/site-packages/requests-stubs/api.pyi requests.api Active",
        "proj/.venv/lib/python3.12/site-packages/stray.py stray Active",
        "proj/.venv/lib/python3.12/site-packages/yaml/__init__.py yaml Active",
        "proj/libs/member/member/__init__.py member Active",
        "proj/src/proj/__init__.py proj Active",
        "proj/src/proj/core.py proj.core Active",
        "proj/tests/test_core.py tests.test_core Active",
        "python/lib/python3.12/json/__init__.py json Active",
        "python/lib/python3.12/os.py os Active",
        "python/lib/python3.12/site-packages/sys_pkg.py  NotImportable",
    ];
    assert_eq!(got, want);
    let google = m.lookup(b"google").expect("google");
    assert_eq!(m.kind(google), Some(ModuleKind::Namespace));

    // RECORD files go through the same map.
    let mut b = ModuleMapBuilder::new();
    let yaml = r.dists.by_name(b"pyyaml").expect("yaml");
    let first = r.add_record_files(&mut b, yaml);
    let m = b.finish();
    assert_eq!(first, 0);
    assert_eq!(m.file_module_name(0), Some(&b"yaml"[..]));
    assert_eq!(m.file_root(1), Some(3));
}

#[test]
fn sources_follow_toggles() {
    let t = fixture();
    let fp = WalkOptions::default();
    let paths = [t.p("proj")];
    let describe = |r: &Roots, srcs: &[Source]| -> Vec<String> {
        srcs.iter()
            .map(|src| match src {
                Source::Walk { path, opts, origin } => {
                    let p = path.strip_prefix(t.path()).expect("under temp").display();
                    format!("walk {p} {origin} gitignore={} excludes={:?}", opts.gitignore, opts.excludes)
                }
                Source::Record { dist } => format!("record {}", s(r.dists.name(*dist))),
            })
            .collect()
    };

    let off = discover(&t.p("proj"), &opts());
    assert!(off.roots.iter().filter(|r| r.origin != Origin::FirstParty).all(|r| !r.analyse));
    assert_eq!(describe(&off, &off.sources(&paths, &fp)), ["walk proj first-party gitignore=true excludes=[]"]);

    let on = discover(&t.p("proj"), &RootOptions { include_deps: true, include_stdlib: true, ..opts() });
    let want = [
        "walk proj first-party gitignore=true excludes=[]",
        "walk python/lib/python3.12 stdlib gitignore=false excludes=[\"/site-packages\"]",
        "record PyYAML",
        "record googleapis-common-protos",
        "record protobuf",
        "record requests",
        "record types-requests",
        "walk proj/.venv/lib/python3.12/site-packages/norecord third-party gitignore=false excludes=[]",
        "walk proj/.venv/lib/python3.12/site-packages/stray.py third-party gitignore=false excludes=[]",
        "walk external/src third-party gitignore=true excludes=[]",
        "walk external/hooked third-party gitignore=true excludes=[]",
    ];
    assert_eq!(describe(&on, &on.sources(&paths, &fp)), want);
}

#[test]
fn overlapping_records_and_nested_roots() {
    let t = TempDir::new("overlap");
    let root = t.path().display().to_string();
    t.file("proj/pyproject.toml", "[project]\nname = \"proj\"\n")
        .file("proj/src/proj/__init__.py", "")
        .file("proj/src/proj/core.py", "")
        .file("proj/src/single.py", "")
        .file("proj/.venv/pyvenv.cfg", "version_info = 3.12.0\n");
    // Conflicting distributions installing the same files.
    dist(&t, SITE, "opencv-python", "4.0", &["cv2/__init__.py", "cv2/data.py"], None, None);
    dist(&t, SITE, "opencv-python-headless", "4.0", &["cv2/__init__.py", "cv2/headless.py"], None, None);
    // pywin32-style relative `.pth` lines naming directories inside site-packages.
    dist(&t, SITE, "pywin32", "306", &["pywin32.pth=win32\nwin32/lib\n", "win32/lib/win32con.py", "win32/winerror.py"], None, None);
    // The project installed through a setuptools import hook mapping onto its own src/.
    let hook_pth = "__editable__.proj-0.1.pth=import __editable___proj_0_1_finder; __editable___proj_0_1_finder.install()\n";
    let finder = format!("__editable___proj_0_1_finder.py=MAPPING = {{'proj': '{root}/proj/src/proj', 'single': '{root}/proj/src/single.py'}}\n");
    dist(&t, SITE, "proj", "0.1", &[hook_pth, &finder], None, None);

    let r = discover(&t.p("proj"), &RootOptions { include_deps: true, ..opts() });
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    let d = &r.dists;
    let (cv, headless) = (d.by_name(b"opencv-python").expect("cv"), d.by_name(b"opencv-python-headless").expect("hl"));
    assert_eq!(d.files(headless).map(s).collect::<Vec<_>>(), ["cv2/__init__.py", "cv2/headless.py"]);
    assert_eq!(d.owned_files(headless).map(s).collect::<Vec<_>>(), ["cv2/headless.py"]);
    assert_eq!(d.owned_files(cv).map(s).collect::<Vec<_>>(), ["cv2/__init__.py", "cv2/data.py"]);

    // RECORD files are classified like walked files: owned files only, `.pth` roots inside
    // site-packages give the module names.
    let mut b = ModuleMapBuilder::new();
    for id in d.ids() {
        r.add_record_files(&mut b, id);
    }
    let m = b.finish();
    let names: Vec<&str> = (0..m.file_count() as u32).map(|i| m.file_module_name(i).map_or("", s)).collect();
    assert_eq!(names, ["cv2", "cv2.data", "cv2.headless", "win32con", "winerror"]);
    let kinds: Vec<RootKind> = (0..m.file_count() as u32).map(|i| r.root(m.file_root(i).expect("root")).kind).collect();
    assert_eq!(kinds[..3], [RootKind::SitePackages; 3]);
    assert_eq!(kinds[3..], [RootKind::Pth; 2]);
    let walked = |rel: &str| {
        let p = t.p(&format!("{SITE}/{rel}"));
        r.classify(&p).map(|l| (r.root(l.root).kind, s(l.rel).to_owned()))
    };
    assert_eq!(walked("win32/lib/win32con.py"), Some((RootKind::Pth, "win32con.py".to_owned())));

    // The project's hook roots exist, but its own files keep the first-party src root.
    let hooks: Vec<&[u8]> = r.roots.iter().filter(|x| x.kind == RootKind::EditableHook).map(|x| &*x.prefix).collect();
    assert_eq!(hooks, [&b"proj"[..], b"single"]);
    for f in ["proj/src/proj/core.py", "proj/src/single.py"] {
        let p = t.p(f);
        let l = r.classify(&p).expect("classified");
        assert_eq!((r.root(l.root).kind, l.origin), (RootKind::Src, Origin::FirstParty), "{f}");
    }
}

/// A venv at `prefix` (relative to `t`) for Python 3.`minor` with one module in site-packages.
fn mini_venv(t: &TempDir, prefix: &str, minor: u8, cfg: &str) {
    t.file(&format!("{prefix}/pyvenv.cfg"), cfg).file(&format!("{prefix}/lib/python3.{minor}/site-packages/m.py"), "");
}

#[test]
fn venv_selection_order() {
    let t = TempDir::new("venvsel");
    t.file("proj/pyproject.toml", "");
    mini_venv(&t, "proj/.venv", 12, "version_info = 3.12.0\n");
    mini_venv(&t, "proj/venv", 11, "version_info = 3.11.0\n");
    mini_venv(&t, "env", 13, "version = 3.13.1\n");
    mini_venv(&t, "configured", 10, "");

    let r = discover(&t.p("proj"), &opts());
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!((venv.prefix.clone(), venv.source), (t.p("proj/.venv"), VenvSource::Project));
    assert_eq!(r.python_version, Some(v(12)));

    let r = discover(&t.p("proj"), &RootOptions { virtual_env: Some(t.p("env")), ..opts() });
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!((venv.prefix.clone(), venv.source), (t.p("env"), VenvSource::Environment));
    assert_eq!(r.python_version, Some(v(13)));

    // Configured (relative to the project root) beats $VIRTUAL_ENV; the version comes from the
    // directory name when pyvenv.cfg has none.
    let r = discover(&t.p("proj"), &RootOptions { venv: Some("../configured".into()), virtual_env: Some(t.p("env")), ..opts() });
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!((venv.prefix.clone(), venv.source), (t.p("configured"), VenvSource::Configured));
    assert_eq!(r.python_version, Some(v(10)));
    assert_eq!(r.version_source, Some(VersionSource::Venv));

    // A configured Python version sets the target version only (see
    // `target_version_differs_from_venv`).
    let r = discover(&t.p("proj"), &RootOptions { python_version: Some(v(12)), ..opts() });
    assert_eq!(r.version_source, Some(VersionSource::Config));
    assert_eq!(r.venv.as_ref().map(|v| v.site_packages.clone()), Some(vec![t.p("proj/.venv/lib/python3.12/site-packages")]));

    // A broken $VIRTUAL_ENV is reported and discovery falls back to .venv.
    let r = discover(&t.p("proj"), &RootOptions { virtual_env: Some(t.p("nope")), ..opts() });
    assert_eq!(r.venv.as_ref().map(|v| v.source), Some(VenvSource::Project));
    assert_eq!(r.diagnostics.len(), 1);
    assert_eq!(r.diagnostics[0].kind, DiagnosticKind::Venv);
    assert!(r.diagnostics[0].to_string().contains("$VIRTUAL_ENV"), "{}", r.diagnostics[0]);

    // A broken configured venv is reported and nothing else is tried.
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("nope")), ..opts() });
    assert!(r.venv.is_none());
    assert_eq!(r.diagnostics.len(), 1);

    // No venv at all, deps requested: reported.
    let lone = TempDir::new("novenv");
    let r = discover(lone.path(), &RootOptions { include_deps: true, ..opts() });
    assert!(r.venv.is_none() && r.dists.is_empty());
    assert_eq!(r.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(), [DiagnosticKind::Venv]);
    assert_eq!(rel_roots(&lone, &r), [(String::new(), RootKind::Project, Origin::FirstParty, true)]);
}

#[test]
fn stdlib_location() {
    let t = TempDir::new("stdlib");
    // Base interpreter with two versions and a free-threaded build.
    t.file("base/lib/python3.12/os.py", "")
        .file("base/lib/python3.13t/os.py", "")
        .file("base/lib/python3.11/notstdlib.py", "")
        .file("base/real/bin/python3.12", "")
        .file("proj/pyproject.toml", "[project]\nrequires-python = \">=3.10\"\n");

    // base-prefix (virtualenv).
    let base = t.p("base").display().to_string();
    mini_venv(&t, "v1", 12, &format!("base-prefix = {base}\nversion_info = 3.12.1\n"));
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("v1")), include_stdlib: true, ..opts() });
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/lib/python3.12")));
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);

    // home pointing at the prefix itself; free-threaded lib dir.
    mini_venv(&t, "v2", 13, &format!("home = {base}\nversion_info = 3.13.0\n"));
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("v2")), ..opts() });
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/lib/python3.13t")));

    // Wanted version not installed: reported only when the stdlib is requested.
    mini_venv(&t, "v3", 11, &format!("home = {base}/bin\nversion_info = 3.11.0\n"));
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("v3")), ..opts() });
    assert!(r.stdlib.is_none() && r.diagnostics.is_empty());
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("v3")), include_stdlib: true, ..opts() });
    assert_eq!(r.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(), [DiagnosticKind::Stdlib]);
    assert!(r.diagnostics[0].message.contains("3.11"));

    // No venv: interpreter on PATH through a symlink (prefix = realpath/../..). The
    // requires-python floor (3.10) is not installed, so any version is accepted.
    fs::create_dir_all(t.p("pathdir")).expect("mkdir");
    std::os::unix::fs::symlink(t.p("base/real/bin/python3.12"), t.p("pathdir/python3")).expect("symlink");
    fs::create_dir_all(t.p("base/real/lib/python3.12")).expect("mkdir");
    fs::write(t.p("base/real/lib/python3.12/os.py"), "").expect("write");
    let path_env = format!("/nonexistent::{}", t.p("pathdir").display());
    let r = discover(&t.p("proj"), &RootOptions { path_env: Some(path_env.into()), include_stdlib: true, ..opts() });
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/real/lib/python3.12")));
    assert_eq!(r.python_version, Some(v(12)));
    assert_eq!(r.version_source, Some(VersionSource::Stdlib));
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(r.roots.iter().map(|r| r.kind).collect::<Vec<_>>(), [RootKind::Project, RootKind::Stdlib]);

    // Conda-style prefix without pyvenv.cfg: its own stdlib and site-packages.
    t.file("conda/lib/python3.10/os.py", "").file("conda/lib/python3.10/site-packages/c.py", "");
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("conda")), ..opts() });
    let venv = r.venv.as_ref().expect("venv");
    assert!(venv.cfg.is_none());
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("conda/lib/python3.10")));
    assert_eq!(r.python_version, Some(v(10)));
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("conda")), ..opts() });
    let site = r.roots.iter().find(|x| x.kind == RootKind::SitePackages).expect("site");
    assert_eq!(site.path, t.p("conda/lib/python3.10/site-packages"));
    // The stdlib root does not swallow its site-packages: longest prefix wins.
    let p = t.p("conda/lib/python3.10/site-packages/c.py");
    assert_eq!(r.classify(&p).map(|l| r.root(l.root).kind), Some(RootKind::SitePackages));
}

#[test]
fn system_site_packages() {
    let t = TempDir::new("syssite");
    let base = t.p("base").display().to_string();
    t.file("base/lib/python3.12/os.py", "").file("base/lib/python3.12/site-packages/global_pkg.py", "").file("proj/pyproject.toml", "");
    mini_venv(&t, "proj/.venv", 12, &format!("home = {base}/bin\nversion_info = 3.12.0\ninclude-system-site-packages = true\n"));
    let r = discover(&t.p("proj"), &opts());
    let sites: Vec<&Path> = r.roots.iter().filter(|x| x.kind == RootKind::SitePackages).map(|x| &*x.path).collect();
    assert_eq!(sites, [&*t.p("proj/.venv/lib/python3.12/site-packages"), &*t.p("base/lib/python3.12/site-packages")]);
    assert_eq!(r.dists.sites().len(), 2);
    let p = t.p("base/lib/python3.12/site-packages/global_pkg.py");
    let l = r.classify(&p).expect("classified");
    assert_eq!((l.origin, l.rel), (Origin::ThirdParty, &b"global_pkg.py"[..]));
}

#[test]
fn target_version_differs_from_venv() {
    let t = TempDir::new("targetver");
    let base = t.p("base").display().to_string();
    let site = "proj/.venv/lib/python3.12/site-packages";
    t.file("proj/pyproject.toml", "")
        .file("base/lib/python3.12/os.py", "")
        .file(&format!("{site}/dep-1.0.dist-info/METADATA"), "Name: dep\nVersion: 1.0\n")
        .file(&format!("{site}/dep-1.0.dist-info/RECORD"), "dep.py,,\n")
        .file(&format!("{site}/dep.py"), "");
    t.file("proj/.venv/pyvenv.cfg", &format!("home = {base}/bin\nversion_info = 3.12.4\n"));

    // Targeting an older Python than the venv runs keeps the venv's site-packages and its
    // distributions; only `python_version` follows the config.
    let r = discover(&t.p("proj"), &RootOptions { python_version: Some(v(11)), include_deps: true, ..opts() });
    assert_eq!((r.python_version, r.version_source), (Some(v(11)), Some(VersionSource::Config)));
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!(venv.version, Some(v(12)));
    assert_eq!(venv.site_packages, [t.p(site)]);
    assert!(r.dists.by_name(b"dep").is_some());
    let sp = r.roots.iter().find(|x| x.kind == RootKind::SitePackages).expect("site root");
    assert_eq!((&*sp.path, sp.analyse), (&*t.p(site), true));
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    // The 3.11 stdlib is not installed: the venv's 3.12 stdlib is used, reported when the
    // stdlib is analysed.
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/lib/python3.12")));
    let r = discover(&t.p("proj"), &RootOptions { python_version: Some(v(11)), include_stdlib: true, ..opts() });
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/lib/python3.12")));
    assert_eq!(r.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(), [DiagnosticKind::Stdlib]);
    let msg = &r.diagnostics[0].message;
    assert!(msg.contains("3.11") && msg.contains("3.12"), "{msg}");

    // An installed target stdlib is preferred; the system site-packages still come from the
    // venv's own interpreter version.
    t.file("base/lib/python3.11/os.py", "").file("base/lib/python3.11/site-packages/old.py", "");
    t.file("base/lib/python3.12/site-packages/global_pkg.py", "");
    t.file("proj/.venv/pyvenv.cfg", &format!("home = {base}/bin\nversion_info = 3.12.4\ninclude-system-site-packages = true\n"));
    let r = discover(&t.p("proj"), &RootOptions { python_version: Some(v(11)), include_stdlib: true, ..opts() });
    assert_eq!(r.stdlib.as_deref(), Some(&*t.p("base/lib/python3.11")));
    let sites: Vec<&Path> = r.roots.iter().filter(|x| x.kind == RootKind::SitePackages).map(|x| &*x.path).collect();
    assert_eq!(sites, [&*t.p(site), &*t.p("base/lib/python3.12/site-packages")]);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);

    // pyvenv.cfg names a version without site-packages: the one present is used, reported.
    mini_venv(&t, "odd", 12, "version_info = 3.13.0\n");
    let r = discover(&t.p("proj"), &RootOptions { venv: Some(t.p("odd")), ..opts() });
    let venv = r.venv.as_ref().expect("venv");
    assert_eq!((venv.version, venv.site_packages.clone()), (Some(v(12)), vec![t.p("odd/lib/python3.12/site-packages")]));
    assert_eq!(r.python_version, Some(v(12)));
    assert_eq!(r.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(), [DiagnosticKind::Venv]);
    let msg = &r.diagnostics[0].message;
    assert!(msg.contains("3.13") && msg.contains("python3.12"), "{msg}");
}

#[test]
fn first_party_layouts() {
    let t = TempDir::new("layouts");
    // Explicit src (relative and absolute), in order; no auto-detection.
    t.file("a/pyproject.toml", "").file("a/src/pkg/__init__.py", "").dir("a/lib");
    let r = discover(&t.p("a"), &RootOptions { src: vec!["lib".into(), t.p("a/src")], ..opts() });
    let kinds: Vec<_> = rel_roots(&t, &r).into_iter().map(|(p, k, ..)| (p, k)).collect();
    assert_eq!(kinds, [("a/lib".to_owned(), RootKind::Configured), ("a/src".to_owned(), RootKind::Configured)]);

    // `src/` that is itself a package is not a src layout; PEP 420 namespace dirs in src are.
    t.file("b/pyproject.toml", "").file("b/src/__init__.py", "").file("b/src/x.py", "");
    assert_eq!(discover(&t.p("b"), &opts()).roots.len(), 1);
    t.file("c/pyproject.toml", "").file("c/src/nspkg/mod.py", "");
    assert_eq!(discover(&t.p("c"), &opts()).roots[0].path, t.p("c/src"));
    t.file("d/pyproject.toml", "").file("d/src/README.md", "").dir("d/src/empty");
    assert_eq!(discover(&t.p("d"), &opts()).roots.len(), 1);
    // Monorepo `src/` of project directories, and non-identifier modules: not a src layout.
    t.file("m/pyproject.toml", "").file("m/src/my-lib/setup.py", "").file("m/src/my-lib/my_lib/__init__.py", "").file("m/src/run-me.py", "");
    assert_eq!(discover(&t.p("m"), &opts()).roots.len(), 1);
    t.file("e/pyproject.toml", "").file("e/src/single.pyi", "");
    assert_eq!(discover(&t.p("e"), &opts()).roots.len(), 2);

    // Build-backend hints come first; missing dirs are skipped.
    t.file("f/pyproject.toml", "[tool.setuptools.packages.find]\nwhere = [\"python\", \"missing\"]\n").file("f/python/p/__init__.py", "");
    let r = discover(&t.p("f"), &opts());
    assert_eq!(rel_roots(&t, &r).into_iter().map(|(p, ..)| p).collect::<Vec<_>>(), ["f/python", "f"]);

    // Invalid pyproject: reported, defaults used.
    t.file("g/pyproject.toml", "[project\n");
    let r = discover(&t.p("g"), &opts());
    assert_eq!(r.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(), [DiagnosticKind::Pyproject]);
    assert!(r.pyproject.is_none());

    // Project root detection.
    t.file("h/pyproject.toml", "").dir("h/deep/er");
    assert_eq!(find_project_root(&t.p("h/deep/er")), t.p("h"));
    let lone = TempDir::new("noproj");
    assert_eq!(find_project_root(lone.path()), lone.path());
}

#[test]
fn distribution_metadata_edge_cases() {
    let t = TempDir::new("dists");
    let site = "proj/.venv/lib/python3.12/site-packages";
    t.file("proj/pyproject.toml", "").file("proj/.venv/pyvenv.cfg", "version_info = 3.12.0\n");
    let site_abs = t.p(site).display().to_string();

    // Legacy egg-info directory: PKG-INFO, installed-files.txt relative to the egg-info dir.
    t.file(&format!("{site}/legacy-1.2-py3.12.egg-info/PKG-INFO"), "Metadata-Version: 1.0\nName: Legacy\nVersion: 1.2\n")
        .file(&format!("{site}/legacy-1.2-py3.12.egg-info/installed-files.txt"), "../legacy/__init__.py\n../legacy/util.py\nPKG-INFO\n../../../../bin/x\n")
        .file(&format!("{site}/legacy-1.2-py3.12.egg-info/top_level.txt"), "legacy\n")
        .file(&format!("{site}/legacy/__init__.py"), "");
    // Single-file egg-info (distutils): metadata only.
    t.file(&format!("{site}/oldie-0.1.egg-info"), "Name: oldie\nVersion: 0.1\n");
    // Headers longer than the 16 KiB prefix: Version comes after a long folded Description.
    let long = format!("Metadata-Version: 1.1\nName: longmeta\nDescription: {}\nVersion: 3.0\n\n", "x\n        ".repeat(4000));
    t.file(&format!("{site}/longmeta-3.0.dist-info/METADATA"), &long).file(&format!("{site}/longmeta-3.0.dist-info/RECORD"), "");
    // Missing METADATA: falls back to the dir name, reported.
    t.file(&format!("{site}/nometa-0.5.dist-info/RECORD"), "nometa.py,,\n");
    // RECORD with quoted, absolute (inside and outside the site), `./`, `..` and pycache paths;
    // stub-only package; extension module at top level; .pth listed.
    let record = format!(
        "\"we,ird/__init__.py\",sha256=x,1\n{site_abs}/absmod.py,,\n/elsewhere/x.py,,\n./dot/m.py,,\nup/../escape.py,,\nwe,ird/__pycache__/a.cpython-312.pyc,,\n\
         fancy-stubs/__init__.pyi,,\n_speedups.cpython-312-darwin.so,,\nfancy.pth,,\nREADME.md,,\nbin-dir/data.txt,,\nfancy-9.dist-info/METADATA,,\n"
    );
    t.file(&format!("{site}/fancy-9.dist-info/METADATA"), "Name: Fancy\nVersion: 9\n").file(&format!("{site}/fancy-9.dist-info/RECORD"), &record);

    let r = discover(&t.p("proj"), &opts());
    let d = &r.dists;
    let names: Vec<(&str, &str)> = d.ids().map(|i| (s(d.name(i)), s(d.version(i)))).collect();
    assert_eq!(names, [("Fancy", "9"), ("Legacy", "1.2"), ("longmeta", "3.0"), ("nometa", "0.5"), ("oldie", "0.1")]);
    let kinds: Vec<_> = r.diagnostics.iter().map(|x| x.kind).collect();
    assert_eq!(kinds, [DiagnosticKind::Metadata], "{:?}", r.diagnostics);

    let legacy = d.by_name(b"legacy").expect("legacy");
    assert!(d.is_egg_info(legacy) && d.has_record(legacy));
    assert_eq!(d.files(legacy).map(s).collect::<Vec<_>>(), ["legacy/__init__.py", "legacy/util.py"]);
    assert_eq!(d.top_level(legacy).map(s).collect::<Vec<_>>(), ["legacy"]);
    let oldie = d.by_name(b"oldie").expect("oldie");
    assert!(d.is_egg_info(oldie) && !d.has_record(oldie));

    let fancy = d.by_name(b"fancy").expect("fancy");
    assert_eq!(d.files(fancy).map(s).collect::<Vec<_>>(), ["we,ird/__init__.py", "absmod.py", "dot/m.py", "fancy-stubs/__init__.pyi"]);
    assert_eq!(d.top_level(fancy).map(s).collect::<Vec<_>>(), ["_speedups", "absmod", "dot", "fancy"]);
    assert_eq!(d.owner(0, b"fancy.pth"), Some(fancy), ".pth files are indexed for ownership");
    assert!(d.is_claimed(0, b"bin-dir") && d.is_claimed(0, b"README.md") && !d.is_claimed(0, b"fancy-9.dist-info"));
    assert!(!d.is_claimed(0, b"elsewhere") && !d.is_claimed(0, b"up"));
    assert_eq!(d.record_hash(d.by_name(b"longmeta").expect("long")), crate::intern::hash_bytes(b""));
}

// ---------------------------------------------------------------------------------------------
// Project units
// ---------------------------------------------------------------------------------------------

#[test]
fn project_units_are_found_by_the_walk_and_files_map_to_the_nearest() {
    let t = TempDir::new("units");
    t.file("pyproject.toml", "[project]\nname = \"mono\"\n")
        .file("a/pyproject.toml", "[project]\nname = \"a\"\n[project.scripts]\na = \"a.cli:main\"\n")
        .file("a/x/pyproject.toml", "[project]\nname = \"ax\"\n")
        .file("a-b/pyproject.toml", "not toml [")
        .file("ignored/pyproject.toml", "[project]\nname = \"ignored\"\n")
        .file(".gitignore", "ignored/\n")
        .file("a/m.py", "");
    let w = crate::walk::walk(&[t.path()], &crate::walk::WalkOptions { projects: true, ..Default::default() });
    assert_eq!(w.files.len(), 1);
    assert_eq!(w.projects.len(), 4, "{:?}", w.projects.iter().collect::<Vec<_>>());
    let root_info = parse_pyproject("[project]\nname = \"mono\"\n[project.scripts]\nm = \"m:main\"\n");
    let (units, diags) = ProjectUnits::discover(t.path(), root_info.as_ref(), &w.projects);
    assert_eq!(units.len(), 4);
    // The invalid one is reported but still a unit.
    assert_eq!(diags.len(), 1);
    assert!(diags[0].path.ends_with("a-b/pyproject.toml"));
    let unit = |rel: &str| {
        let d = t.p(rel);
        units.dir(units.unit_of_dir(d.as_os_str().as_encoded_bytes())).strip_prefix(t.path()).expect("under root").to_owned()
    };
    assert_eq!(unit("a"), Path::new("a"));
    assert_eq!(unit("a/pkg/sub"), Path::new("a"));
    assert_eq!(unit("a/x/y"), Path::new("a/x"));
    // `a-b` sorts between `a` and `a/...` bytewise: still its own unit, and `a-c` is the root's.
    assert_eq!(unit("a-b/q"), Path::new("a-b"));
    assert_eq!(unit("a-c"), Path::new(""));
    assert_eq!(unit("ignored"), Path::new(""));
    assert_eq!(units.unit_of_dir(b"/elsewhere/entirely"), 0);
    // Scripts per unit; the root keeps its parsed ones.
    assert_eq!(units.scripts(0), ["m:main"]);
    let a = units.unit_of_dir(t.p("a").as_os_str().as_encoded_bytes());
    assert_eq!(units.scripts(a), ["a.cli:main"]);
    assert_eq!(units.all_scripts().len(), 2);
    // Without the walk's list: one unit.
    let single = ProjectUnits::single(t.path(), None);
    assert_eq!((single.len(), single.unit_of_dir(t.p("a").as_os_str().as_encoded_bytes())), (1, 0));
}
