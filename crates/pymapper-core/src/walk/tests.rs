//! Tests for the walker and the gitignore matcher.

use super::wildmatch::wildmatch;
use super::*;

// ---------------------------------------------------------------------------------------------
// wildmatch: cases from git's t/t3070-wildmatch.sh ("glob" column, i.e. WM_PATHNAME).

fn wm(pat: &str, text: &str) -> bool {
    wildmatch(pat.as_bytes(), text.as_bytes())
}

#[test]
fn wildmatch_basics() {
    let yes = [
        ("foo", "foo"),
        ("", ""),
        ("???", "foo"),
        ("*", "foo"),
        ("f*", "foo"),
        ("*foo*", "foo"),
        ("*ob*a*r*", "foobar"),
        ("*ab", "aaaaaaabababab"),
        ("foo\\*", "foo*"),
        ("f\\\\oo", "f\\oo"),
        ("*[al]?", "ball"),
        ("**[!te]", "ten"),
        ("t[a-g]n", "ten"),
        ("t[!a-g]n", "ton"),
        ("t[^a-g]n", "ton"),
        ("a[]]b", "a]b"),
        ("a[]-]b", "a-b"),
        ("a[]-]b", "a]b"),
        ("a[]a-]b", "aab"),
        ("]", "]"),
        ("foo**bar", "foobazbar"),
        ("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo-bar"),
        ("\\!x", "!x"),
        ("\\#x", "#x"),
    ];
    for (p, t) in yes {
        assert!(wm(p, t), "{p:?} should match {t:?}");
    }
    let no = [
        ("bar", "foo"),
        ("??", "foo"),
        ("*f", "foo"),
        ("foo\\*bar", "foobar"),
        ("[ten]", "ten"),
        ("**[!ten]", "ten"),
        ("t[!a-g]n", "ten"),
        ("a[]-]b", "aab"),
        ("foo", "fo"),
        ("fo", "foo"),
        // A trailing lone backslash never matches, an unterminated class never matches.
        ("\\", "\\"),
        ("[", "["),
        ("[a", "a"),
    ];
    for (p, t) in no {
        assert!(!wm(p, t), "{p:?} should not match {t:?}");
    }
}

#[test]
fn wildmatch_slashes_and_double_star() {
    let yes = [
        ("foo/**/bar", "foo/baz/bar"),
        ("foo/**/**/bar", "foo/baz/bar"),
        ("foo/**/bar", "foo/b/a/z/bar"),
        ("foo/**/bar", "foo/bar"),
        ("foo/*", "foo/bar"),
        ("**/foo", "foo"),
        ("**/foo", "XXX/foo"),
        ("**/foo", "bar/baz/foo"),
        ("**/bar/*", "deep/foo/bar/baz"),
        ("**/bar/**", "deep/foo/bar/baz/"),
        ("**/bar/**", "deep/foo/bar/"),
        ("*/bar/**", "foo/bar/baz/x"),
        ("**/bar/*/*", "deep/foo/bar/baz/x"),
        ("XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*", "XXX/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1"),
        ("**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt"),
        ("*/*/*", "foo/bba/arr"),
        ("**/**/**", "foo/bb/aa/rr"),
        ("*X*i", "abcXdefXghi"),
        ("*/*X*/*/*i", "ab/cXd/efXg/hi"),
        ("**/*X*/**/*i", "ab/cXd/efXg/hi"),
        ("a/**", "a/b/c"),
        ("**", "a/b/c"),
        ("-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1"),
    ];
    for (p, t) in yes {
        assert!(wm(p, t), "{p:?} should match {t:?}");
    }
    let no = [
        ("foo*bar", "foo/baz/bar"),
        ("foo**bar", "foo/baz/bar"),
        ("foo?bar", "foo/bar"),
        ("foo[/]bar", "foo/bar"),
        ("foo[^a-z]bar", "foo/bar"),
        ("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo/bar"),
        ("*/foo", "bar/baz/foo"),
        ("**/bar*", "foo/bar/baz"),
        ("**/bar/*", "deep/foo/bar/baz/"),
        ("**/bar/*", "deep/foo/bar"),
        ("**/bar**", "foo/bar/baz"),
        ("*/bar/**", "deep/foo/bar/baz/x"),
        ("**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txtz"),
        ("*/*/*", "foo/bb/aa/rr"),
        ("*X*i", "ab/cXd/efXg/hi"),
        ("*", "a/b"),
        ("a/**", "a"),
        ("-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-X-70-iso8859-1"),
    ];
    for (p, t) in no {
        assert!(!wm(p, t), "{p:?} should not match {t:?}");
    }
}

#[test]
fn wildmatch_classes() {
    assert!(wm("[[:alpha:]][[:digit:]][[:upper:]]", "a1B"));
    assert!(!wm("[[:digit:][:upper:][:space:]]", "a"));
    assert!(wm("[[:digit:][:upper:][:space:]]", "A"));
    assert!(wm("[[:digit:][:upper:][:space:]]", "1"));
    assert!(wm("[[:digit:][:upper:][:space:]]", " "));
    assert!(!wm("[[:digit:][:upper:][:spaci:]]", "1"), "unknown class aborts");
    assert!(wm("[[:xdigit:]]", "f"));
    assert!(!wm("[[:xdigit:]]", "g"));
    assert!(wm("[[:punct:]]", "."));
    assert!(wm("[a-c[:digit:]x-z]", "5"));
    assert!(wm("[a-c[:digit:]x-z]", "b"));
    assert!(wm("[a-c[:digit:]x-z]", "y"));
    assert!(!wm("[a-c[:digit:]x-z]", "q"));
    assert!(wm("[\\]]", "]"));
    assert!(wm("[\\-]", "-"));
    assert!(wm("[!\\]]", "a"));
    // "[[:" without ":]" is an ordinary '[' member.
    assert!(wm("[[:a]", "["));
}

#[test]
fn wildmatch_is_not_exponential() {
    let text = "a".repeat(200);
    let start = std::time::Instant::now();
    assert!(!wm("*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b", &text));
    assert!(!wm("**/*a*a*a*a*a*a*a*a*a*a*a*a*b", &format!("{text}/{text}/{text}")));
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "took {:?}", start.elapsed());
}

// ---------------------------------------------------------------------------------------------
// Gitignore matcher semantics (gitignore(5)).

/// `Match` of `path` (a directory when it ends in `/`) against the ignore file `src`.
fn gi_match(src: &str, path: &str) -> Match {
    let gi = Gitignore::parse(src.as_bytes());
    match path.strip_suffix('/') {
        Some(dir) => gi.matched(dir.as_bytes(), true),
        None => gi.matched(path.as_bytes(), false),
    }
}

/// Asserts which of `paths` the ignore file `src` ignores (`I`), whitelists (`W`) or leaves
/// alone (`-`).
#[track_caller]
fn check(src: &str, cases: &[(&str, char)]) {
    for &(path, want) in cases {
        let got = match gi_match(src, path) {
            Match::Ignore => 'I',
            Match::Whitelist => 'W',
            Match::None => '-',
        };
        assert_eq!(got, want, "ignore file {src:?}, path {path:?}");
    }
}

#[test]
fn gitignore_comments_blanks_and_escapes() {
    let src = "# comment\n\n   \n\\#hash\n\\!bang\nspace\\ \ntrail   \n";
    let gi = Gitignore::parse(src.as_bytes());
    assert_eq!(gi.len(), 4, "comment and blank lines are dropped");
    check(src, &[("# comment", '-'), ("#hash", 'I'), ("!bang", 'I'), ("space ", 'I'), ("space", '-')]);
    check(src, &[("trail", 'I'), ("trail   ", '-')]);
    // A tab is not trimmed; "!" alone, "/" alone and "!/" never match anything.
    check("tab\t\n!\n/\n!/\n", &[("tab\t", 'I'), ("tab", '-'), ("x", '-')]);
    // A '#' that is not the first byte is part of the pattern.
    check("a#b\n", &[("a#b", 'I')]);
}

#[test]
fn gitignore_crlf_and_bom() {
    let src = "\u{feff}first\r\n*.log\r\nlast";
    check(src, &[("first", 'I'), ("x.log", 'I'), ("last", 'I'), ("x.log\r", '-')]);
    // Without a newline at the end the last line still counts.
    check("a\nb", &[("b", 'I')]);
}

#[test]
fn gitignore_negation_last_match_wins() {
    let src = "*.py\n!keep.py\n";
    check(src, &[("a.py", 'I'), ("keep.py", 'W'), ("sub/keep.py", 'W'), ("sub/a.py", 'I'), ("a.txt", '-')]);
    // Order matters: a later exclude beats an earlier negation.
    check("!keep.py\n*.py\n", &[("keep.py", 'I')]);
    // Literal, suffix and glob patterns interleaved with negations.
    let src = "build\n!build\n*.pyc\n!a.pyc\na?c.pyc\n";
    check(src, &[("build", 'W'), ("x.pyc", 'I'), ("a.pyc", 'W'), ("abc.pyc", 'I')]);
    // An escaped '!' is a literal, not a negation.
    check("\\!x\n", &[("!x", 'I'), ("x", '-')]);
}

#[test]
fn gitignore_basename_vs_anchored() {
    // No slash: matches the basename at any depth.
    check("foo\n", &[("foo", 'I'), ("a/foo", 'I'), ("a/b/foo/", 'I'), ("foobar", '-'), ("a/xfoo", '-')]);
    check("*.py\n", &[("x.py", 'I'), ("a/b/x.py", 'I'), ("x.pyi", '-')]);
    // Leading slash: anchored to the ignore file's directory.
    check("/foo\n", &[("foo", 'I'), ("foo/", 'I'), ("a/foo", '-')]);
    check("/*.py\n", &[("x.py", 'I'), ("a/x.py", '-')]);
    // Slash in the middle: anchored too.
    check("a/foo\n", &[("a/foo", 'I'), ("x/a/foo", '-'), ("a/b/foo", '-')]);
    check("doc/*.txt\n", &[("doc/x.txt", 'I'), ("doc/sub/x.txt", '-'), ("y/doc/x.txt", '-')]);
    // `*` and `?` never cross a slash.
    check("a*b\n", &[("ab", 'I'), ("axxb", 'I'), ("d/axb", 'I')]);
    check("a/*/b\n", &[("a/x/b", 'I'), ("a/x/y/b", '-'), ("a/b", '-')]);
    check("a?b/c\n", &[("axb/c", 'I'), ("a/b/c", '-')]);
}

#[test]
fn gitignore_directory_only() {
    let src = "build/\n";
    check(src, &[("build/", 'I'), ("a/build/", 'I'), ("build", '-'), ("a/build", '-')]);
    let src = "doc/frotz/\n";
    check(src, &[("doc/frotz/", 'I'), ("doc/frotz", '-'), ("a/doc/frotz/", '-')]);
    let src = "*.egg-info/\n";
    check(src, &[("x.egg-info/", 'I'), ("x.egg-info", '-')]);
    // A directory-only negation does not re-include a file.
    check("*.py\n!keep.py/\n", &[("keep.py", 'I'), ("keep.py/", 'W')]);
}

#[test]
fn gitignore_double_star() {
    // Leading `**/`: in all directories, including the top.
    check("**/foo\n", &[("foo", 'I'), ("a/foo", 'I'), ("a/b/foo/", 'I'), ("afoo", '-')]);
    check("**/foo/bar\n", &[("foo/bar", 'I'), ("x/y/foo/bar", 'I'), ("foo/x/bar", '-')]);
    // Trailing `/**`: everything inside, but not the directory itself.
    check("abc/**\n", &[("abc/x", 'I'), ("abc/x/y.py", 'I'), ("abc", '-'), ("abc/", '-'), ("x/abc/y", '-')]);
    // `/**/`: zero or more directories.
    check("a/**/b\n", &[("a/b", 'I'), ("a/x/b", 'I'), ("a/x/y/b", 'I'), ("a/xb", '-'), ("b", '-')]);
    // Anything else is a plain `*`.
    check("foo**bar\n", &[("foobazbar", 'I'), ("foo/baz/bar", '-')]);
    check("**\n", &[("x", 'I'), ("a/b/c.py", 'I')]);
    // `**` combined with negation re-includes inside a non-excluded directory.
    check("abc/**\n!abc/keep.py\n", &[("abc/keep.py", 'W'), ("abc/other.py", 'I')]);
}

#[test]
fn gitignore_classes_and_case() {
    check("[ab].py\n", &[("a.py", 'I'), ("b.py", 'I'), ("c.py", '-')]);
    check("[!ab].py\n", &[("c.py", 'I'), ("a.py", '-')]);
    check("x[0-9]\n", &[("x5", 'I'), ("xa", '-')]);
    // Case-sensitive, like git with core.ignorecase=false.
    check("Build\n*.PY\n", &[("Build", 'I'), ("build", '-'), ("x.PY", 'I'), ("x.py", '-')]);
    // Escaped metacharacters are literals.
    check("\\*.py\nq\\?\n", &[("*.py", 'I'), ("x.py", '-'), ("q?", 'I'), ("qa", '-')]);
}

#[test]
fn gitignore_from_patterns_and_empty() {
    let gi = Gitignore::from_patterns(&["build/", "*.tmp", "!keep.tmp"]);
    assert_eq!(gi.len(), 3);
    assert_eq!(gi.matched(b"a/build", true), Match::Ignore);
    assert_eq!(gi.matched(b"x.tmp", false), Match::Ignore);
    assert_eq!(gi.matched(b"keep.tmp", false), Match::Whitelist);
    let empty = Gitignore::parse(b"# only comments\n\n");
    assert!(empty.is_empty());
    assert_eq!(empty.matched(b"anything", false), Match::None);
    // Non-UTF-8 bytes are matched as bytes.
    let gi = Gitignore::parse(b"caf\xe9*\n");
    assert_eq!(gi.matched(b"caf\xe9.py", false), Match::Ignore);
    assert_eq!(gi.matched(b"cafe.py", false), Match::None);
}

// ---------------------------------------------------------------------------------------------
// Walker.

use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-walk-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Creates the file `rel` (and its parent directories) with `contents`.
    fn file(&self, rel: &str, contents: &str) -> &TempDir {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        self
    }

    /// Creates the directory `rel`.
    fn dir(&self, rel: &str) -> &TempDir {
        fs::create_dir_all(self.0.join(rel)).expect("mkdir");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Restore permissions changed by tests so the tree can be removed.
        let _ = std::process::Command::new("chmod").arg("-R").arg("u+rwx").arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Walks `root` and returns the files relative to it, in walk order.
fn rel_files(root: &Path, opts: &WalkOptions) -> Vec<String> {
    let w = walk(&[root], opts);
    assert!(w.diagnostics.is_empty(), "unexpected diagnostics: {:?}", w.diagnostics);
    rel_list(root, &w.files)
}

fn rel_list(root: &Path, files: &PathList) -> Vec<String> {
    files
        .iter()
        .map(|p| p.strip_prefix(root).expect("below root").to_str().expect("utf-8").to_owned())
        .collect()
}

fn opts() -> WalkOptions {
    WalkOptions::default()
}

#[test]
fn walk_yields_python_files_sorted() {
    let t = TempDir::new("basic");
    t.file("b.py", "")
        .file("a.pyi", "")
        .file("a.py", "")
        .file("pkg/__init__.py", "")
        .file("pkg/sub/m.py", "")
        .file("pkg-x.py", "")
        .file("readme.md", "")
        .file("x.pyc", "")
        .file("x.py.bak", "")
        .file("nb.ipynb", "")
        .dir("empty")
        .dir("dir.py");
    // Component-wise order: `pkg/...` sorts before `pkg-x.py`.
    assert_eq!(
        rel_files(t.path(), &opts()),
        ["a.py", "a.pyi", "b.py", "pkg/__init__.py", "pkg/sub/m.py", "pkg-x.py"]
    );
    let w = walk(&[t.path()], &opts());
    assert_eq!(w.stats.dirs, 5, "root, pkg, pkg/sub, empty, dir.py");
    assert_eq!(w.stats.entries, 14);
}

#[test]
fn walk_skips_pycache_git_and_hidden_dirs() {
    let t = TempDir::new("skip");
    t.file("__pycache__/c.py", "")
        .file("a/__pycache__/c.py", "")
        .file(".git/hooks/h.py", "")
        .file(".venv/lib/v.py", "")
        .file(".tox/t.py", "")
        .file(".hidden.py", "")
        .file("a/ok.py", "");
    assert_eq!(rel_files(t.path(), &opts()), [".hidden.py", "a/ok.py"]);
    let w = walk(&[t.path()], &opts());
    assert_eq!(w.stats.hidden, 2, ".venv and .tox");
    let all = WalkOptions { hidden: true, ..opts() };
    assert_eq!(rel_files(t.path(), &all), [".hidden.py", ".tox/t.py", ".venv/lib/v.py", "a/ok.py"]);
}

#[test]
fn walk_does_not_follow_symlinks() {
    let t = TempDir::new("links");
    let outside = TempDir::new("links-target");
    outside.file("o.py", "").file("d/od.py", "");
    t.file("real.py", "").file("pkg/m.py", "");
    let root = t.path();
    std::os::unix::fs::symlink(root.join("real.py"), root.join("link.py")).unwrap();
    std::os::unix::fs::symlink(root.join("pkg"), root.join("linkdir")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
    std::os::unix::fs::symlink(root, root.join("pkg/loop")).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), root.join("dangling.py")).unwrap();
    assert_eq!(rel_files(root, &opts()), ["pkg/m.py", "real.py"]);
    assert_eq!(walk(&[root], &opts()).stats.symlinks, 5);
    // A root that is a symlink is followed (like `find -H`); links below it are not.
    let via = TempDir::new("links-via");
    let link_root = via.path().join("root");
    std::os::unix::fs::symlink(root, &link_root).unwrap();
    assert_eq!(rel_files(&link_root, &opts()), ["pkg/m.py", "real.py"]);
}

#[test]
fn walk_roots_files_missing_and_dedup() {
    let t = TempDir::new("roots");
    t.file("a/x.py", "").file("b/y.py", "").file("notes.txt", "");
    let root = t.path();
    let missing = root.join("nope");
    // A file root is yielded whatever its extension; overlapping roots are deduplicated.
    let w = walk(&[root.join("a"), root.join("notes.txt"), missing.clone(), root.to_path_buf()], &opts());
    assert_eq!(rel_list(root, &w.files), ["a/x.py", "b/y.py", "notes.txt"]);
    assert_eq!(w.diagnostics.len(), 1);
    assert_eq!(w.diagnostics[0].kind, DiagnosticKind::Root);
    assert_eq!(w.diagnostics[0].path, missing);
    assert!(w.diagnostics[0].to_string().starts_with("cannot read root"));
    // A trailing slash on the root does not double it in the output.
    let slashed = format!("{}/", root.join("a").display());
    let w = walk(&[slashed.as_str()], &opts());
    assert_eq!(w.files.get(0), root.join("a/x.py"));
    // No roots, no files.
    let none: [&Path; 0] = [];
    assert!(walk(&none, &opts()).files.is_empty());
}

#[test]
fn walk_nested_gitignore_files() {
    let t = TempDir::new("nested-gi");
    t.file(".gitignore", "*.gen.py\nbuild/\n/top_only.py\nlib/vendored/\n")
        .file("a.py", "")
        .file("a.gen.py", "")
        .file("top_only.py", "")
        .file("build/b.py", "")
        .file("pkg/top_only.py", "")
        .file("pkg/build", "")
        .file("pkg/build.py", "")
        .file("lib/vendored/v.py", "")
        .file("lib/ok.py", "")
        .file("x/lib/vendored/v.py", "")
        // A deeper file overrides: re-include one generated file, ignore more names.
        .file("pkg/.gitignore", "!keep.gen.py\nlocal.py\n/anchored.py\nsub/*.py\n!sub/yes.py\n")
        .file("pkg/keep.gen.py", "")
        .file("pkg/drop.gen.py", "")
        .file("pkg/local.py", "")
        .file("pkg/deep/local.py", "")
        .file("pkg/anchored.py", "")
        .file("pkg/deep/anchored.py", "")
        .file("pkg/sub/no.py", "")
        .file("pkg/sub/yes.py", "")
        .file("pkg/sub/inner/z.py", "")
        .file("local.py", "");
    assert_eq!(
        rel_files(t.path(), &opts()),
        [
            "a.py",
            "lib/ok.py",
            "local.py",
            "pkg/build.py",
            "pkg/deep/anchored.py",
            "pkg/keep.gen.py",
            "pkg/sub/inner/z.py",
            "pkg/sub/yes.py",
            "pkg/top_only.py",
            "x/lib/vendored/v.py",
        ]
    );
    let w = walk(&[t.path()], &opts());
    assert_eq!(w.stats.ignore_files, 2);
    // `build/` (a directory) and `lib/vendored/` are pruned, not listed.
    assert_eq!(w.stats.dirs, 9, "root, lib, x, x/lib, x/lib/vendored, pkg, pkg/deep, pkg/sub, pkg/sub/inner");
}

#[test]
fn walk_cannot_reinclude_below_an_ignored_directory() {
    let t = TempDir::new("reinclude");
    t.file(".gitignore", "gen/\nout/*\n!out/keep.py\n")
        .file("gen/.gitignore", "!*.py\n")
        .file("gen/a.py", "")
        .file("out/keep.py", "")
        .file("out/drop.py", "")
        .file("out/sub/s.py", "");
    // `gen/` is pruned, so its own `!*.py` is never read; `out/*` leaves `out` itself listed.
    assert_eq!(rel_files(t.path(), &opts()), ["out/keep.py"]);
}

#[test]
fn walk_gitignore_can_be_disabled() {
    let t = TempDir::new("gi-off");
    t.file(".gitignore", "*\n").file(".git/info/exclude", "*\n").file("a.py", "").file("d/b.py", "");
    assert!(rel_files(t.path(), &opts()).is_empty());
    let off = WalkOptions { gitignore: false, ..opts() };
    assert_eq!(rel_files(t.path(), &off), ["a.py", "d/b.py"]);
    assert_eq!(walk(&[t.path()], &off).stats.ignore_files, 0);
}

#[test]
fn walk_info_exclude_has_lowest_precedence() {
    let t = TempDir::new("exclude");
    t.dir(".git/info")
        .file(".git/info/exclude", "# local\n*.local.py\nscratch/\n")
        .file(".gitignore", "!keep.local.py\n")
        .file("a.py", "")
        .file("x.local.py", "")
        .file("keep.local.py", "")
        .file("scratch/s.py", "")
        .file("d/y.local.py", "");
    assert_eq!(rel_files(t.path(), &opts()), ["a.py", "keep.local.py"]);
    // A missing info/exclude is fine.
    let t = TempDir::new("exclude-missing");
    t.dir(".git").file("a.py", "");
    assert_eq!(rel_files(t.path(), &opts()), ["a.py"]);
}

#[test]
fn walk_root_below_repository_top_uses_parent_ignore_files() {
    let t = TempDir::new("parents");
    t.dir(".git/info")
        .file(".git/info/exclude", "excluded.py\n")
        .file(".gitignore", "/src/pkg/anchored.py\n*.tmp.py\n")
        .file("src/.gitignore", "pkg/sub/\n!keep.tmp.py\n")
        .file("src/pkg/a.py", "")
        .file("src/pkg/anchored.py", "")
        .file("src/pkg/x.tmp.py", "")
        .file("src/pkg/keep.tmp.py", "")
        .file("src/pkg/excluded.py", "")
        .file("src/pkg/sub/s.py", "")
        .file("src/pkg/.gitignore", "own.py\n")
        .file("src/pkg/own.py", "");
    let root = t.path().join("src/pkg");
    assert_eq!(rel_files(&root, &opts()), ["a.py", "keep.tmp.py"]);
    let w = walk(&[&root], &opts());
    assert_eq!(w.stats.ignore_files, 4, "exclude, two parent files, own file");
    // Without parents only the root's own ignore file applies.
    let no_parents = WalkOptions { parents: false, ..opts() };
    assert_eq!(
        rel_files(&root, &no_parents),
        ["a.py", "anchored.py", "excluded.py", "keep.tmp.py", "sub/s.py", "x.tmp.py"]
    );
    // The repository top itself as root sees the same results for that subtree.
    let w = walk(&[t.path()], &opts());
    assert_eq!(rel_list(t.path(), &w.files), ["src/pkg/a.py", "src/pkg/keep.tmp.py"]);
}

#[test]
fn walk_nested_repository_resets_ignore_rules() {
    let t = TempDir::new("nested-repo");
    t.file(".gitignore", "*.py\n!top.py\n")
        .file("top.py", "")
        .file("other.py", "")
        .dir("vendor/lib/.git/info")
        .file("vendor/lib/.git/info/exclude", "secret.py\n")
        .file("vendor/lib/a.py", "")
        .file("vendor/lib/secret.py", "")
        .file("vendor/lib/.gitignore", "gen.py\n")
        .file("vendor/lib/gen.py", "")
        .file("vendor/other.py", "");
    assert_eq!(rel_files(t.path(), &opts()), ["top.py", "vendor/lib/a.py"]);
}

#[test]
fn walk_worktree_dot_git_file_points_at_common_dir() {
    let t = TempDir::new("worktree");
    // Layout of `git worktree add`: <main>/.git/worktrees/wt holds `commondir` = "../..".
    t.dir("main/.git/worktrees/wt")
        .file("main/.git/worktrees/wt/commondir", "../..\n")
        .file("main/.git/info/exclude", "excluded.py\n")
        .file("wt/.git", "gitdir: ../main/.git/worktrees/wt\n")
        .file("wt/a.py", "")
        .file("wt/excluded.py", "")
        .file("wt/sub/excluded.py", "");
    assert_eq!(rel_files(&t.path().join("wt"), &opts()), ["a.py"]);
    // Root below the worktree top: found through the parent chain.
    t.file("wt/sub/b.py", "");
    assert_eq!(rel_files(&t.path().join("wt/sub"), &opts()), ["b.py"]);
    // Submodule layout: absolute gitdir, no commondir.
    let abs = t.path().join("modules/sm");
    t.file("modules/sm/info/exclude", "x.py\n")
        .file("sm/.git", &format!("gitdir: {}\n", abs.display()))
        .file("sm/x.py", "")
        .file("sm/y.py", "");
    assert_eq!(rel_files(&t.path().join("sm"), &opts()), ["y.py"]);
}

#[test]
fn walk_configured_excludes() {
    let t = TempDir::new("excludes");
    t.file(".gitignore", "!*.py\n")
        .file("a.py", "")
        .file("tests/t.py", "")
        .file("src/tests/t.py", "")
        .file("src/m.py", "")
        .file("src/m_test.py", "")
        .file("docs/conf.py", "")
        .file("x/docs/conf.py", "");
    let ex = WalkOptions { excludes: vec!["tests/".into(), "*_test.py".into(), "/docs".into()], ..opts() };
    // Excludes win over gitignore negations; `/docs` is anchored to the root.
    assert_eq!(rel_files(t.path(), &ex), ["a.py", "src/m.py", "x/docs/conf.py"]);
    // Relative to each root, also when the root is below a repository top.
    t.dir(".git");
    assert!(rel_files(&t.path().join("x"), &ex).is_empty());
    // A later negation cancels an earlier configured exclude.
    let neg = WalkOptions { excludes: vec!["*.py".into(), "!m.py".into()], ..opts() };
    assert_eq!(rel_files(&t.path().join("src"), &neg), ["m.py"]);
    let w = walk(&[t.path()], &ex);
    assert_eq!(w.stats.ignored, 4, "tests, src/tests, src/m_test.py, docs");
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn walk_errors_become_diagnostics() {
    let t = TempDir::new("diag");
    t.file("a.py", "")
        .file("locked/l.py", "")
        .file("open/o.py", "")
        .file("gi/.gitignore", "x.py\n")
        .file("gi/x.py", "")
        .file("gi/y.py", "");
    chmod(&t.path().join("locked"), 0o000);
    chmod(&t.path().join("gi/.gitignore"), 0o000);
    if fs::read_dir(t.path().join("locked")).is_ok() {
        eprintln!("skipping: permissions are not enforced (running as root?)");
        return;
    }
    let w = walk(&[t.path()], &opts());
    // The walk carries on: everything readable is still found.
    assert_eq!(rel_list(t.path(), &w.files), ["a.py", "gi/x.py", "gi/y.py", "open/o.py"]);
    let kinds: Vec<(DiagnosticKind, PathBuf)> = w.diagnostics.iter().map(|d| (d.kind, d.path.clone())).collect();
    assert_eq!(
        kinds,
        [
            (DiagnosticKind::IgnoreFile, t.path().join("gi/.gitignore")),
            (DiagnosticKind::ReadDir, t.path().join("locked")),
        ]
    );
    assert_eq!(w.diagnostics[1].error.kind(), io::ErrorKind::PermissionDenied);
    assert!(w.diagnostics[1].to_string().contains("cannot list directory"));
    // An unreadable root is a diagnostic too.
    let w = walk(&[t.path().join("locked")], &opts());
    assert!(w.files.is_empty());
    assert_eq!(w.diagnostics.len(), 1);
    assert_eq!(w.diagnostics[0].kind, DiagnosticKind::ReadDir);
}

#[test]
fn walk_non_utf8_names() {
    let t = TempDir::new("bytes");
    let name = OsStr::from_bytes(b"caf\xe9.py");
    // APFS refuses non-UTF-8 names; nothing to test there.
    if fs::write(t.path().join(name), "").is_err() {
        return;
    }
    t.file(".gitignore", "x*.py\n");
    let w = walk(&[t.path()], &opts());
    assert_eq!(w.files.len(), 1);
    assert_eq!(w.files.get(0), t.path().join(name));
}

/// Plain recursive reference walk: same rules, no gitignore, sequential.
fn reference_walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("read_dir") {
        let e = e.expect("entry");
        let ty = e.file_type().expect("type");
        let name = e.file_name();
        let b = name.as_bytes();
        if ty.is_dir() {
            if b != b"__pycache__" && b[0] != b'.' {
                reference_walk(&e.path(), out);
            }
        } else if ty.is_file() && is_python(b) {
            out.push(e.path());
        }
    }
}

#[test]
fn walk_parallel_matches_sequential_reference() {
    let t = TempDir::new("wide");
    // 3 levels x 6 dirs x 8 files: 259 directories, 2072 files.
    let mut stack = vec![(t.path().to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        fs::create_dir_all(&dir).unwrap();
        for f in 0..8 {
            let ext = ["py", "pyi", "txt", "py"][f % 4];
            fs::write(dir.join(format!("m{f}.{ext}")), "").unwrap();
        }
        if depth < 3 {
            for d in 0..6 {
                stack.push((dir.join(format!("d{d}")), depth + 1));
            }
        }
    }
    fs::create_dir_all(t.path().join("d0/__pycache__")).unwrap();
    fs::write(t.path().join("d0/__pycache__/c.py"), "").unwrap();
    let mut want = Vec::new();
    reference_walk(t.path(), &mut want);
    want.sort_by(|a, b| path_cmp(a.as_os_str().as_bytes(), b.as_os_str().as_bytes()));
    for threads in [1, 3, 8] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let w = pool.install(|| walk(&[t.path()], &opts()));
        assert!(w.diagnostics.is_empty());
        let got: Vec<PathBuf> = w.files.iter().map(Path::to_path_buf).collect();
        assert_eq!(got.len(), 259 * 6);
        assert_eq!(got, want, "{threads} threads");
        assert_eq!(w.stats.dirs, 259, "{threads} threads");
    }
    // Unsorted output has the same set.
    let w = walk(&[t.path()], &WalkOptions { sort: false, ..opts() });
    let mut got: Vec<PathBuf> = w.files.iter().map(Path::to_path_buf).collect();
    got.sort_by(|a, b| path_cmp(a.as_os_str().as_bytes(), b.as_os_str().as_bytes()));
    assert_eq!(got, want);
}

#[test]
fn path_list_and_order() {
    let mut l = PathList::default();
    assert!(l.is_empty());
    for p in ["a/b.py", "a-b.py", "a.py", "a/b.py", "/x", "a/a/z.py", ""] {
        l.push(p.as_bytes()).unwrap();
    }
    assert_eq!(l.len(), 7);
    assert_eq!(l.byte_len(), 6 + 6 + 4 + 6 + 2 + 8);
    assert_eq!(l.get(4), Path::new("/x"));
    assert_eq!(l.get_bytes(6), b"");
    assert_eq!(path_cmp(b"a/b", b"a-b"), Ordering::Less);
    assert_eq!(path_cmp(b"a", b"a/b"), Ordering::Less);
    assert_eq!(path_cmp(b"ab", b"a/b"), Ordering::Greater);
    assert_eq!(path_cmp(b"x", b"x"), Ordering::Equal);
}

/// A worker result from `(root, rel)` pairs.
fn found(items: &[(u32, &str)]) -> Found {
    let mut f = Found::default();
    for &(r, rel) in items {
        f.push(r, &[rel.as_bytes()]).unwrap();
    }
    f
}

#[test]
fn merge_joins_prefixes_sorts_and_dedups() {
    // Roots: "/r/" , "/r/sub/" (nested in the first), "/q/", and the file-root slot "".
    let prefixes: [&[u8]; 4] = [b"/r/", b"/r/sub/", b"/q/", b""];
    let a = found(&[(0, "sub/m.py"), (0, "z.py"), (1, "m.py"), (0, "a-b.py")]);
    let b = found(&[(2, "x.py"), (1, "n.py"), (0, "sub/n.py"), (3, "/r/z.py"), (0, "a/b.py")]);
    let (out, dropped) = merge(vec![a, Found::default(), b], &prefixes, true);
    assert_eq!(dropped, 0);
    let got: Vec<&str> = out.iter().map(|p| p.to_str().unwrap()).collect();
    assert_eq!(got, ["/q/x.py", "/r/a/b.py", "/r/a-b.py", "/r/sub/m.py", "/r/sub/n.py", "/r/z.py"]);
    // Exactly sized: no growth slack in the output.
    assert_eq!(out.bytes.capacity(), out.byte_len() + "/r/sub/m.py".len() + "/r/sub/n.py".len() + "/r/z.py".len());
    // Unsorted: concatenation in worker order, duplicates kept.
    let a = found(&[(0, "z.py"), (1, "m.py")]);
    let (cat, _) = merge(vec![a, found(&[(3, "/f.py")])], &prefixes, false);
    let got: Vec<&str> = cat.iter().map(|p| p.to_str().unwrap()).collect();
    assert_eq!(got, ["/r/z.py", "/r/sub/m.py", "/f.py"]);
}

#[test]
fn joined_cmp_agrees_with_concatenation() {
    const PARTS: &[&str] = &["", "a", "/", "a/", "ab", "a-", "a/b", "b", "/a", "aa/"];
    for a0 in PARTS {
        for a1 in PARTS {
            for b0 in PARTS {
                for b1 in PARTS {
                    let want = path_cmp(format!("{a0}{a1}").as_bytes(), format!("{b0}{b1}").as_bytes());
                    let got = joined_cmp(a0.as_bytes(), a1.as_bytes(), b0.as_bytes(), b1.as_bytes());
                    assert_eq!(got, want, "{a0:?}+{a1:?} vs {b0:?}+{b1:?}");
                }
            }
        }
    }
}

#[test]
fn root_prefixes() {
    let p = |s: &str| String::from_utf8(root_prefix(s.as_bytes()).into_vec()).unwrap();
    assert_eq!(p("."), "");
    assert_eq!(p("./"), "");
    assert_eq!(p("src"), "src/");
    assert_eq!(p("src//"), "src/");
    assert_eq!(p("/"), "/");
    assert_eq!(p("/a/b/"), "/a/b/");
}

#[test]
fn python_names() {
    assert!(is_python(b"a.py"));
    assert!(is_python(b"a.pyi"));
    assert!(!is_python(b"a.pyc"));
    assert!(!is_python(b"a.pyw"));
    assert!(!is_python(b"a.py.bak"));
    assert!(!is_python(b"a.ipynb"));
    assert!(!is_python(b"A.PY"));
}

// ---------------------------------------------------------------------------------------------
// Differential test against git itself (needs a `git` binary, so it is ignored by default):
//   cargo test -p pymapper-core walk_agrees_with_git -- --ignored

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const GEN_DIRS: &[&str] = &["a", "b", "build", "src", ".h", "d.py", "lib"];
const GEN_FILES: &[&str] = &["a.py", "b.py", "keep.py", "t.pyi", "test_a.py", ".h.py", "lib.py", "x.py"];
const GEN_PATS: &[&str] = &[
    "*.py", "!*.py", "a.py", "!a.py", "build/", "build", "/a", "a/", "a/b", "**/b", "a/**", "a/**/x.py",
    "!keep.py", "*.pyi", "!b/", "src/*.py", "/*.py", "test_*", "!test_a.py", ".h", "d.py/", "d.py",
    "*/a.py", "lib/**/*.py", "[ab].py", "?.py", "!src", "b/**", "**/lib", "!lib/", "\\!x.py", "x.py ",
];

fn gen_ignore(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..1 + rng.below(4) {
        s.push_str(rng.pick(GEN_PATS));
        s.push('\n');
    }
    s
}

fn gen_tree(rng: &mut Rng, dir: &Path, depth: u32) {
    fs::create_dir_all(dir).unwrap();
    for _ in 0..1 + rng.below(4) {
        fs::write(dir.join(rng.pick(GEN_FILES)), "").unwrap();
    }
    if rng.below(10) < 4 {
        fs::write(dir.join(".gitignore"), gen_ignore(rng)).unwrap();
    }
    if depth < 3 {
        for _ in 0..rng.below(4) {
            let sub = dir.join(rng.pick(GEN_DIRS));
            gen_tree(rng, &sub, depth + 1);
        }
    }
}

/// Untracked, non-ignored Python files according to git, relative to `repo`, sorted.
fn git_untracked(repo: &Path, home: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(repo)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git ls-files failed: {}", String::from_utf8_lossy(&out.stderr));
    let mut v: Vec<String> = out
        .stdout
        .split(|&c| c == 0)
        .filter(|p| !p.is_empty())
        .filter(|p| is_python(p.rsplit(|&c| c == b'/').next().unwrap_or(p)))
        .map(|p| String::from_utf8(p.to_vec()).unwrap())
        .collect();
    v.sort_by(|a, b| path_cmp(a.as_bytes(), b.as_bytes()));
    v
}

#[test]
#[ignore = "needs a git binary; run with --ignored"]
fn walk_agrees_with_git() {
    let home = TempDir::new("git-home");
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let all = WalkOptions { hidden: true, ..opts() };
    let (mut found, mut ignored) = (0usize, 0usize);
    for case in 0..300 {
        let t = TempDir::new("git-diff");
        let repo = t.path();
        let init = std::process::Command::new("git").args(["init", "-q"]).current_dir(repo).status();
        assert!(init.expect("run git").success());
        gen_tree(&mut rng, repo, 0);
        if rng.below(10) < 3 {
            fs::write(repo.join(".git/info/exclude"), gen_ignore(&mut rng)).unwrap();
        }
        let want = git_untracked(repo, home.path());
        let got = rel_files(repo, &all);
        let unfiltered = rel_files(repo, &WalkOptions { gitignore: false, ..all.clone() });
        found += got.len();
        ignored += unfiltered.len() - got.len();
        if got != want {
            let listing = std::process::Command::new("find").arg(".").current_dir(repo).output().unwrap();
            let mut ignores = String::new();
            for line in String::from_utf8_lossy(&listing.stdout).lines() {
                if line.ends_with(".gitignore") || line.ends_with("exclude") {
                    let text = fs::read_to_string(repo.join(line)).unwrap_or_default();
                    ignores.push_str(&format!("--- {line}\n{text}"));
                }
            }
            panic!("case {case}: walker {got:?}\ngit {want:?}\n{ignores}");
        }
    }
    eprintln!("agreed with git on 300 trees: {found} files kept, {ignored} ignored");
    assert!(found > 1000 && ignored > 1000, "generator too weak: {found} kept, {ignored} ignored");
}

// ---------------------------------------------------------------------------------------------
// wildmatch against a plain backtracking reference without git's abort codes and fast skips,
// so the pruning in `dowild`/`star_loop` is checked to never lose a match.

/// Reference matcher: `pat[p..]` against `text[t..]` by exhaustive backtracking.
fn naive_wm(pat: &[u8], p: usize, text: &[u8], t: usize) -> bool {
    if p == pat.len() {
        return t == text.len();
    }
    let rest_t = &text[t..];
    match pat[p] {
        b'\\' => p + 1 < pat.len() && rest_t.first() == Some(&pat[p + 1]) && naive_wm(pat, p + 2, text, t + 1),
        b'?' => rest_t.first().is_some_and(|&c| c != b'/') && naive_wm(pat, p + 1, text, t + 1),
        b'[' => match rest_t.first() {
            None => false,
            Some(&c) => match super::wildmatch::class(pat, p, c) {
                Some((end, true)) => naive_wm(pat, end + 1, text, t + 1),
                _ => false,
            },
        },
        b'*' => {
            let mut q = p;
            while q < pat.len() && pat[q] == b'*' {
                q += 1;
            }
            let seg_start = p == 0 || pat[p - 1] == b'/';
            let seg_end = q == pat.len() || pat[q] == b'/' || (pat[q] == b'\\' && pat.get(q + 1) == Some(&b'/'));
            if q - p >= 2 && seg_start && seg_end {
                // `**` as a whole segment: any run of bytes; `**/` also matches zero dirs.
                if q == pat.len() {
                    return true;
                }
                if pat[q] == b'/' && naive_wm(pat, q + 1, text, t) {
                    return true;
                }
                (t..=text.len()).any(|j| naive_wm(pat, q, text, j))
            } else {
                // Any run of non-`/` bytes.
                let mut j = t;
                loop {
                    if naive_wm(pat, q, text, j) {
                        return true;
                    }
                    if j == text.len() || text[j] == b'/' {
                        return false;
                    }
                    j += 1;
                }
            }
        }
        c => rest_t.first() == Some(&c) && naive_wm(pat, p + 1, text, t + 1),
    }
}

#[test]
fn wildmatch_agrees_with_backtracking_reference() {
    const PAT: &[&str] = &["a", "b", "/", "*", "**", "?", "[ab]", "[!a]", "\\a", "\\/", "ab", "[", "\\"];
    const TXT: &[u8] = b"aab/";
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut matches = 0u32;
    for _ in 0..200_000 {
        let mut pat = Vec::new();
        for _ in 0..1 + rng.below(6) {
            pat.extend_from_slice(rng.pick(PAT).as_bytes());
        }
        let text: Vec<u8> = (0..rng.below(9)).map(|_| TXT[rng.below(TXT.len())]).collect();
        let want = naive_wm(&pat, 0, &text, 0);
        matches += u32::from(want);
        assert_eq!(
            wildmatch(&pat, &text),
            want,
            "pattern {:?} text {:?}",
            String::from_utf8_lossy(&pat),
            String::from_utf8_lossy(&text)
        );
    }
    assert!(matches > 5_000, "generator too weak: {matches} matches");
}

// ---------------------------------------------------------------------------------------------
// `.ignore` files, exclude base, pattern validation, config options.

#[test]
fn walk_dot_ignore_beats_gitignore_of_same_directory() {
    let t = TempDir::new("dot-ignore");
    t.file(".gitignore", "*.pyi\nkeep.py\n")
        .file(".ignore", "!keep.py\ngen.py\n")
        .file("keep.py", "")
        .file("gen.py", "")
        .file("a.py", "")
        .file("s.pyi", "")
        // A deeper `.gitignore` beats a shallower `.ignore`.
        .file("sub/.gitignore", "!gen.py\n")
        .file("sub/gen.py", "");
    assert_eq!(rel_files(t.path(), &opts()), ["a.py", "keep.py", "sub/gen.py"]);
    // Switched off together with gitignore.
    let off = WalkOptions { gitignore: false, ..opts() };
    assert_eq!(rel_files(t.path(), &off), ["a.py", "gen.py", "keep.py", "s.pyi", "sub/gen.py"]);
    // Parent `.ignore` files apply to a root below the repository top.
    t.dir(".git");
    t.file("sub/x/x.py", "").file("sub/x/gen.py", "");
    assert_eq!(rel_files(&t.path().join("sub/x"), &opts()), ["gen.py", "x.py"]);
    t.file("sub/.ignore", "x.py\n");
    assert_eq!(rel_files(&t.path().join("sub/x"), &opts()), ["gen.py"]);
}

#[test]
fn walk_excludes_relative_to_exclude_base() {
    let t = TempDir::new("ex-base");
    t.file("src/pkg/gen/g.py", "")
        .file("src/pkg/m.py", "")
        .file("src/pkg/gen.py", "")
        .file("other/gen/o.py", "")
        .file("tests/gen/t.py", "");
    let ex = |base: Option<PathBuf>| WalkOptions {
        excludes: vec!["src/pkg/gen/".into(), "/tests".into()],
        exclude_base: base,
        ..opts()
    };
    let based = ex(Some(t.path().to_path_buf()));
    // Anchored patterns see the path from the project root, whichever root is walked.
    assert_eq!(rel_files(t.path(), &based), ["other/gen/o.py", "src/pkg/gen.py", "src/pkg/m.py"]);
    assert_eq!(rel_files(&t.path().join("src/pkg"), &based), ["gen.py", "m.py"]);
    assert_eq!(rel_files(&t.path().join("src"), &based), ["pkg/gen.py", "pkg/m.py"]);
    // Without a base they are relative to each root, so `src/pkg/gen/` matches nothing here.
    assert_eq!(rel_files(&t.path().join("src/pkg"), &ex(None)), ["gen/g.py", "gen.py", "m.py"]);
    // An unnormalised base is resolved before comparing.
    let messy = t.path().join("src/../.");
    assert_eq!(rel_files(&t.path().join("src/pkg"), &ex(Some(messy))), ["gen.py", "m.py"]);
    // A base reached through a symlink still lines up (canonical fallback).
    let link = t.path().with_extension("link");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(t.path(), &link).unwrap();
    assert_eq!(rel_files(&t.path().join("src/pkg"), &ex(Some(link.clone()))), ["gen.py", "m.py"]);
    fs::remove_file(&link).unwrap();
    // A root outside the base matches the patterns relative to itself.
    let outside = TempDir::new("ex-outside");
    outside.file("tests/x.py", "").file("y.py", "");
    assert_eq!(rel_files(outside.path(), &based), ["y.py"]);
}

#[test]
fn gitignore_check_pattern() {
    for ok in ["build/", "*.py", "!keep.py", "\\#x", "a/**/b", "[ab]", "[[:alpha:]]x", "\\\\", "a\\ "] {
        assert_eq!(Gitignore::check_pattern(ok.as_bytes()), Ok(()), "{ok:?}");
    }
    let bad = [
        ("#x", PatternError::Comment),
        ("", PatternError::Empty),
        ("!", PatternError::Empty),
        ("/", PatternError::Empty),
        ("   ", PatternError::Empty),
        ("a\\", PatternError::TrailingBackslash),
        ("[ab", PatternError::BadClass),
        ("x[[:nope:]]", PatternError::BadClass),
        ("[\\", PatternError::BadClass),
    ];
    for (pat, want) in bad {
        assert_eq!(Gitignore::check_pattern(pat.as_bytes()), Err(want), "{pat:?}");
    }
    assert!(PatternError::Comment.to_string().contains("comment"));
}

#[test]
fn walk_reports_invalid_excludes() {
    let t = TempDir::new("bad-ex");
    t.file("a.py", "").file("b.py", "");
    let o = WalkOptions { excludes: vec!["#a.py".into(), "b.py".into(), "[x".into()], ..opts() };
    let w = walk(&[t.path()], &o);
    // The comment does not exclude `a.py`; valid patterns still apply.
    assert_eq!(rel_list(t.path(), &w.files), ["a.py"]);
    let got: Vec<(DiagnosticKind, &Path)> = w.diagnostics.iter().map(|d| (d.kind, d.path.as_path())).collect();
    assert_eq!(got, [(DiagnosticKind::Exclude, Path::new("#a.py")), (DiagnosticKind::Exclude, Path::new("[x"))]);
    assert_eq!(w.diagnostics[0].error.kind(), io::ErrorKind::InvalidInput);
    assert!(w.diagnostics[1].to_string().starts_with("invalid exclude pattern [x: "));
}

#[test]
fn walk_options_from_config() {
    let root = Path::new("/proj");
    let mut cfg = Config::new(root);
    cfg.extend_exclude.push("gen".into());
    cfg.respect_gitignore = false;
    let o = WalkOptions::first_party(&cfg, root);
    assert!(!o.gitignore && o.parents && !o.hidden && o.sort);
    assert_eq!(o.exclude_base.as_deref(), Some(root));
    assert_eq!(o.excludes.last().map(String::as_str), Some("gen"));
    assert_eq!(o.excludes.len(), crate::config::DEFAULT_EXCLUDES.len() + 1);
    // The default excludes are all valid patterns.
    for p in crate::config::DEFAULT_EXCLUDES {
        assert_eq!(Gitignore::check_pattern(p.as_bytes()), Ok(()), "{p}");
    }
    let e = WalkOptions::everything();
    assert!(!e.gitignore && !e.parents && e.hidden && e.excludes.is_empty());
}

#[test]
fn walk_lists_config_files_when_asked() {
    let t = TempDir::new("configs");
    t.file("a.py", "")
        .file("servers/elnb.yaml", "module_name: x\n")
        .file("cfg/app.toml", "")
        .file("data.json", "{}")
        .file("setup.cfg", "")
        .file("tox.ini", "")
        .file("notes.txt", "")
        .file("uv.lock", "")
        .file("web/package-lock.json", "{}")
        .file("ignored/secret.yaml", "")
        .file("excluded/x.yml", "")
        .file(".hidden/h.yaml", "")
        .file(".gitignore", "ignored/\n");
    let globs: Vec<String> = crate::config::DEFAULT_CONFIG_EVIDENCE_GLOBS.iter().map(|g| (*g).to_owned()).collect();
    let o = WalkOptions { configs: globs, excludes: vec!["excluded/".to_owned()], ..opts() };
    let w = walk(&[t.path()], &o);
    assert!(w.diagnostics.is_empty(), "{:?}", w.diagnostics);
    // Lockfiles, gitignored, excluded and hidden files are skipped; sorted component-wise.
    assert_eq!(rel_list(t.path(), &w.configs), ["cfg/app.toml", "data.json", "servers/elnb.yaml", "setup.cfg", "tox.ini"]);
    assert_eq!(rel_list(t.path(), &w.files), ["a.py"]);
    // Off by default.
    assert!(walk(&[t.path()], &opts()).configs.is_empty());
    assert!(is_lockfile(b"poetry.lock") && !is_lockfile(b"pyproject.toml"));
}
