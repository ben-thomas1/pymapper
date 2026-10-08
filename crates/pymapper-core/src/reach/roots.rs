//! Root sets: the nodes that are live without being called.

use crate::config::{EntrypointMatcher, Mode};
use crate::extract::facts::{DefKind, FileView, def_flags, file_flags, import_flags};
use crate::index::{FileId, Index, NodeId, NodeKind};
use crate::intern::Sym;
use crate::resolve::{Global, Imports, ResolvedCalls, res_flags};
use crate::roots::Origin;

/// Why a node is a root (bit flags; a node can have several).
pub mod root_kind {
    /// The `<module>` pseudo-scope of a file (import-time code).
    pub const MODULE: u8 = 1 << 0;
    /// A module with an `if __name__ == "__main__":` block, or a `__main__` module.
    pub const MAIN: u8 = 1 << 1;
    /// A test (test function, `Test*` class, unittest/pytest hook, conftest function).
    pub const TEST: u8 = 1 << 2;
    /// Matched an entrypoint decorator glob.
    pub const DECORATOR: u8 = 1 << 3;
    /// Matched an entrypoint name glob.
    pub const NAME: u8 = 1 << 4;
    /// A subclass (or method of a subclass) of an entrypoint base class.
    pub const BASE: u8 = 1 << 5;
    /// A `[project.scripts]` / `entry_points.txt` target.
    pub const SCRIPT: u8 = 1 << 6;
    /// Public API (library mode).
    pub const PUBLIC: u8 = 1 << 7;

    /// Names of the bits, lowest first.
    pub const NAMES: [&str; 8] = ["module", "main", "test", "decorator", "name", "base", "script", "public"];
}

/// Which roots to collect. [`RootOptions::application`] and [`RootOptions::library`] are the
/// two dead-code modes (design: dead code (e)).
#[derive(Clone, Copy, Debug)]
pub struct RootOptions<'a> {
    /// Bit `1 << origin as u8` set: files of that [`Origin`] contribute roots. Default:
    /// first-party only (third-party and stdlib code is live only through first-party code).
    pub origins: u8,
    /// Every `<module>` pseudo-scope is a root (module-level code runs on import).
    pub modules: bool,
    /// `__main__` guards and `__main__` modules.
    pub main: bool,
    /// Tests: test files (`test_*.py`, `*_test.py`, `test.py`, `conftest.py`, anything under
    /// a `test` / `tests` package) and in them functions and methods named `test*`, classes named
    /// `Test*`, unittest / pytest hooks, and every module-level function of `conftest.py`
    /// (fixtures and hooks are found by name). Deliberately lenient: `test*` methods of any
    /// class of a test file are roots, not only of `Test*` / `unittest.TestCase` classes, and
    /// a package named `test` / `tests` at any level above the module makes a test file (the
    /// distribution, depth 0, is not checked).
    pub tests: bool,
    /// Library mode: the public API is live (see [`roots`]).
    pub public_api: bool,
    /// Declarative entrypoint rules (decorator, base class and name globs, presets included).
    pub entrypoints: Option<&'a EntrypointMatcher>,
    /// Script targets, `pkg.mod:attr.path` or `pkg.mod` (extras such as `[cli]` and spaces are
    /// ignored), from `[project.scripts]` and `entry_points.txt`.
    pub scripts: &'a [&'a [u8]],
}

impl RootOptions<'_> {
    /// Application mode: modules, `__main__`, tests; no public API.
    pub fn application() -> RootOptions<'static> {
        RootOptions {
            origins: 1 << Origin::FirstParty as u8,
            modules: true,
            main: true,
            tests: true,
            public_api: false,
            entrypoints: None,
            scripts: &[],
        }
    }

    /// Library mode: application roots plus the public API.
    pub fn library() -> RootOptions<'static> {
        RootOptions { public_api: true, ..RootOptions::application() }
    }

    #[inline]
    fn includes(&self, o: Origin) -> bool {
        self.origins & (1 << o as u8) != 0
    }
}

/// Whether `mode` means library mode. `Auto` is application when the project declares script
/// targets (`has_scripts`) or a first-party file has a `__main__` guard or is a `__main__`
/// module; library otherwise. O(files).
pub fn is_library(ix: &Index, mode: Mode, has_scripts: bool) -> bool {
    match mode {
        Mode::Application => false,
        Mode::Library => true,
        Mode::Auto => {
            if has_scripts {
                return false;
            }
            let main = (0..ix.file_count()).any(|f| {
                ix.file_origin(f) == Origin::FirstParty
                    && (ix.file_facts(f).has(file_flags::MAIN_GUARD) || ix.name(ix.file_node(f)) == b"__main__")
            });
            !main
        }
    }
}

/// The roots of an index: one [`root_kind`] byte per node (0 = not a root).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RootSet {
    kinds: Vec<u8>,
    count: u32,
    /// Script targets that matched no node (as given).
    unmatched: Vec<u32>,
}

impl RootSet {
    /// An empty root set over `n` nodes.
    pub fn new(n: u32) -> RootSet {
        RootSet { kinds: vec![0; n as usize], count: 0, unmatched: Vec::new() }
    }

    /// Marks `v` as a root of kind `kind` (a [`root_kind`] bit).
    #[inline]
    pub fn add(&mut self, v: NodeId, kind: u8) {
        let k = &mut self.kinds[v as usize];
        if *k == 0 && kind != 0 {
            self.count += 1;
        }
        *k |= kind;
    }

    /// [`root_kind`] bits of `v` (0 if `v` is not a root).
    #[inline]
    pub fn kind(&self, v: NodeId) -> u8 {
        self.kinds[v as usize]
    }

    #[inline]
    pub fn contains(&self, v: NodeId) -> bool {
        self.kinds[v as usize] != 0
    }

    /// Number of nodes the set covers (roots or not).
    #[inline]
    pub fn node_count(&self) -> u32 {
        self.kinds.len() as u32
    }

    /// Number of roots.
    #[inline]
    pub fn len(&self) -> u32 {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Roots, ascending.
    pub fn iter(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.kinds.iter().enumerate().filter(|(_, k)| **k != 0).map(|(v, _)| v as NodeId)
    }

    /// Indices into [`RootOptions::scripts`] of targets that matched no node.
    pub fn unmatched_scripts(&self) -> &[u32] {
        &self.unmatched
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.kinds.capacity() + self.unmatched.capacity() * 4
    }
}

/// unittest / pytest hooks that run without being called by the code (methods or functions of
/// test files).
const TEST_HOOKS: &[&[u8]] = &[
    b"setUp",
    b"tearDown",
    b"setUpClass",
    b"tearDownClass",
    b"setUpModule",
    b"tearDownModule",
    b"asyncSetUp",
    b"asyncTearDown",
    b"setup_method",
    b"teardown_method",
    b"setup_class",
    b"teardown_class",
    b"setup_module",
    b"teardown_module",
    b"setup_function",
    b"teardown_function",
    b"setup",
    b"teardown",
];

/// Reused per-file buffers of [`roots`].
#[derive(Default)]
struct Scratch {
    /// Dotted names / qualified ids being matched.
    buf: Vec<u8>,
    /// Per def of the current file: is public API (library mode).
    public: Vec<bool>,
    /// Sorted `__all__` of the current file.
    exports: Vec<Sym>,
}

/// Collects the roots of `ix` (see [`RootOptions`]). O(files + defs + decorators), plus, with
/// base-class entrypoint rules, O(classes + bases + MRO entries) and one qualified name per
/// class. Nothing is allocated per node besides the result (one byte per node).
///
/// - `<module>` roots are the package/module nodes of files.
/// - Library mode (`public_api`): a module is public when no package/module on its path
///   starts with `_` (dunders such as `__main__` excepted); its node is a root, and so is every
///   module-level class/function that is not private, restricted to `__all__` when the module
///   defines a static one, then recursively every non-private method of a public class. With
///   `imp`, targets of a public module's re-exports (`from ._impl import x` listed in
///   `__all__`, `import x as x`, public names of an `__init__` without `__all__`) are roots
///   too, wherever they are defined.
/// - Entrypoint rules: a decorator glob matches the decorator's dotted name as written
///   (`app.route`); a name glob the simple name or the qualified id. Base-class globs need
///   `calls` (class MROs): a class is a root when one of its written bases, expanded through
///   the file's imports with `imp` (`models.Model` after `from django.db import models` is
///   `django.db.models.Model`), or a loaded ancestor's dotted name (`pkg.mod.Class`) matches;
///   its methods are roots too.
/// - Script targets are looked up as qualified ids ([`Index::find`]), then, with `imp`,
///   through module-level bindings (re-exports in an `__init__`); the target, the module
///   named by the target and the module defining the target are roots.
pub fn roots(ix: &Index, imp: Option<&Imports>, calls: Option<&ResolvedCalls>, opts: &RootOptions<'_>) -> RootSet {
    let mut set = RootSet::new(ix.node_count());
    let mut sc = Scratch::default();
    let matcher = opts.entrypoints.filter(|m| !m.is_empty());
    for f in 0..ix.file_count() {
        if opts.includes(ix.file_origin(f)) {
            file_roots(ix, imp, f, opts, matcher, &mut set, &mut sc);
        }
    }
    if let (Some(mt), Some(calls)) = (matcher, calls)
        && mt.has_bases()
    {
        base_roots(ix, imp, calls, mt, opts, &mut set, &mut sc.buf);
    }
    for (i, s) in opts.scripts.iter().enumerate() {
        sc.buf.clear();
        let target = s.split(|&c| c == b'[').next().unwrap_or(&[]);
        sc.buf.extend(target.iter().copied().filter(|c| !c.is_ascii_whitespace()));
        match script_target(ix, imp, &sc.buf) {
            Some((m, v)) => {
                set.add(m, root_kind::SCRIPT);
                set.add(v, root_kind::SCRIPT);
                if let Some(f) = ix.file(v) {
                    set.add(ix.file_node(f), root_kind::SCRIPT);
                }
            }
            None => set.unmatched.push(i as u32),
        }
    }
    set
}

/// Resolves script target `pkg.mod[:attr.path]` (whitespace and extras removed) to its module
/// node and its target node. The attribute path is first looked up as a qualified id
/// ([`Index::find`]). Failing that, each segment is resolved, with `imp`, as a module-level
/// binding of the current module (`Imports::global`: re-exports such as `from .cli import main`
/// in `pkg/__init__.py`, aliases, star imports), else as a child (a def of a class or module,
/// a submodule of a package). Targets that are
/// not `module[:path]` (empty, `@group`, several `:`) match nothing.
fn script_target(ix: &Index, imp: Option<&Imports>, target: &[u8]) -> Option<(NodeId, NodeId)> {
    if target.is_empty() || target[0] == b'@' || target.iter().filter(|&&c| c == b':').count() > 1 {
        return None;
    }
    let (module, path) = match target.iter().position(|&c| c == b':') {
        Some(i) => (&target[..i], Some(&target[i + 1..])),
        None => (target, None),
    };
    let m = ix.find(module)?;
    if !matches!(ix.kind(m), NodeKind::Package | NodeKind::Module) {
        return None;
    }
    let Some(path) = path else { return Some((m, m)) };
    if let Some(v) = ix.find(target) {
        return Some((m, v));
    }
    let mut cur = m;
    for seg in path.split(|&c| c == b'.') {
        let sym = ix.strings().get(seg)?;
        let module_like = matches!(ix.kind(cur), NodeKind::Package | NodeKind::Module);
        let via_binding = match (module_like, imp) {
            (true, Some(imp)) => match ix.file(cur).and_then(|f| imp.global(ix, f, sym)) {
                Some(Global::Def(n) | Global::Module(n)) => Some(n),
                _ => None,
            },
            _ => None,
        };
        cur = match via_binding {
            Some(n) => n,
            // A def of a class, or a submodule of a package (`from pkg import sub` semantics).
            None => ix.children(cur).find(|&c| (ix.kind(c).is_def() || module_like) && ix.name_sym(c) == sym)?,
        };
    }
    Some((m, cur))
}

/// A dunder name (`__x__`).
#[inline]
fn is_dunder(name: &[u8]) -> bool {
    name.len() > 4 && name.starts_with(b"__") && name.ends_with(b"__")
}

/// No package/module on the path of module node `m` is private.
fn module_is_public(ix: &Index, m: NodeId) -> bool {
    (1..=ix.depth(m)).all(|k| {
        let name = ix.name(ix.ancestor_at(m, k));
        !name.starts_with(b"_") || is_dunder(name)
    })
}

/// A test module: `test_*`, `*_test`, `test`, `tests`, `conftest`, or below a `test` /
/// `tests` package.
fn is_test_module(ix: &Index, m: NodeId) -> bool {
    let name = ix.name(m);
    if name.starts_with(b"test_") || name.ends_with(b"_test") || matches!(name, b"test" | b"tests" | b"conftest") {
        return true;
    }
    let d = ix.depth(m);
    (1..d).any(|k| matches!(ix.name(ix.ancestor_at(m, k)), b"test" | b"tests"))
}

/// Joins a name chain with `.` into `buf` (appending).
fn push_dotted(ix: &Index, chain: &[Sym], buf: &mut Vec<u8>) {
    for (i, &s) in chain.iter().enumerate() {
        if i > 0 {
            buf.push(b'.');
        }
        buf.extend_from_slice(ix.str(s));
    }
}

#[allow(clippy::too_many_arguments)]
fn file_roots(
    ix: &Index,
    imp: Option<&Imports>,
    f: FileId,
    opts: &RootOptions<'_>,
    matcher: Option<&EntrypointMatcher>,
    set: &mut RootSet,
    sc: &mut Scratch,
) {
    let m = ix.file_node(f);
    let ff = ix.file_facts(f);
    let view = ix.file_view(f);
    let defs = &ix.facts().defs;
    let base = ff.defs.start as usize;
    let n = ff.defs.len as usize;
    let mname = ix.name(m);
    if opts.modules {
        set.add(m, root_kind::MODULE);
    }
    if opts.main && (ff.has(file_flags::MAIN_GUARD) || mname == b"__main__") {
        set.add(m, root_kind::MAIN);
    }
    let test_file = opts.tests && is_test_module(ix, m);
    let conftest = test_file && mname == b"conftest";
    if test_file {
        // The test runner imports test modules.
        set.add(m, root_kind::TEST);
    }
    let lib = opts.public_api && module_is_public(ix, m);
    let restrict = lib && ff.has(file_flags::ALL_DEFINED) && !ff.has(file_flags::ALL_DYNAMIC);
    if lib {
        set.add(m, root_kind::PUBLIC);
        sc.public.clear();
        sc.public.resize(n, false);
        sc.public[0] = true;
        if restrict {
            sc.exports.clear();
            sc.exports.extend_from_slice(view.exports());
            sc.exports.sort_unstable();
        }
    }
    for d in 1..n {
        let row = base + d;
        let v = m + d as u32;
        let kind = DefKind::from_u8(defs.kind[row]);
        let flags = defs.flags[row];
        let parent = defs.parent[row] as usize;
        let at_module = parent == 0;
        let in_class = !at_module && DefKind::from_u8(defs.kind[base + parent]) == DefKind::Class;
        let name = ix.name(v);
        if test_file {
            let hit = match kind {
                DefKind::Class => at_module && name.starts_with(b"Test"),
                DefKind::Function => {
                    (at_module || in_class)
                        && (name.starts_with(b"test")
                            || TEST_HOOKS.contains(&name)
                            || (at_module && (conftest || name.starts_with(b"pytest_")))
                            || is_fixture(ix, &view, d as u32, defs.decorators_len[row]))
                }
                DefKind::Module => false,
            };
            if hit {
                set.add(v, root_kind::TEST);
            }
        }
        if lib && flags & def_flags::PRIVATE == 0 {
            let public = if at_module {
                !restrict || defs.name[row].is_some_and(|s| sc.exports.binary_search(&s).is_ok())
            } else {
                in_class && sc.public[parent]
            };
            if public {
                sc.public[d] = true;
                set.add(v, root_kind::PUBLIC);
            }
        }
        if let Some(mt) = matcher {
            if defs.decorators_len[row] > 0 {
                for r in view.decorators(d as u32) {
                    let chain = view.decorator_chain(r);
                    if chain.is_empty() {
                        continue;
                    }
                    sc.buf.clear();
                    push_dotted(ix, chain, &mut sc.buf);
                    if mt.decorator(&sc.buf) {
                        set.add(v, root_kind::DECORATOR);
                        break;
                    }
                }
            }
            let by_name = mt.simple_name(name)
                || (mt.has_qualified_names() && {
                    sc.buf.clear();
                    ix.qid(v, &mut sc.buf);
                    mt.qualified_name(&sc.buf)
                });
            if by_name {
                set.add(v, root_kind::NAME);
            }
        }
    }
    if lib && let Some(imp) = imp {
        for row in ff.imports.range() {
            if imp.flags(row) & res_flags::EXPORTED != 0
                && let Some(t) = imp.target(row)
                && ix.kind(t) != NodeKind::Distribution
            {
                set.add(t, root_kind::PUBLIC);
            }
        }
    }
}

/// Def `d` has a decorator whose last segment is `fixture` (`@pytest.fixture`, `@fixture`,
/// `@pytest.fixture(scope=...)`).
fn is_fixture(ix: &Index, view: &FileView<'_>, d: u32, decorators: u8) -> bool {
    decorators > 0 && view.decorators(d).any(|r| view.decorator_chain(r).last().is_some_and(|&s| ix.str(s) == b"fixture"))
}

/// Dotted name of a node for base matching: its qualified id with `:` as `.`.
fn push_node_dotted(ix: &Index, v: NodeId, buf: &mut Vec<u8>) {
    let at = buf.len();
    ix.qid(v, buf);
    for c in &mut buf[at..] {
        if *c == b':' {
            *c = b'.';
        }
    }
}

/// A written base chain of a class in file `f`, its head expanded through the file's
/// module-level bindings (see [`roots`]).
fn push_expanded_base(ix: &Index, imp: Option<&Imports>, f: FileId, chain: &[Sym], buf: &mut Vec<u8>) {
    let head = chain[0];
    let ff = ix.file_facts(f);
    match imp.and_then(|i| i.global(ix, f, head)) {
        Some(Global::Def(n) | Global::Module(n)) => push_node_dotted(ix, n, buf),
        Some(Global::Unresolved(row)) if ff.imports.range().contains(&(row as usize)) => {
            let row = row as usize;
            let st = &ix.facts().imports;
            let view = ix.file_view(f);
            let module = view.import_module(row);
            if st.flags[row] & import_flags::FROM == 0 {
                if st.alias[row].is_some() {
                    push_dotted(ix, module, buf);
                } else {
                    buf.extend_from_slice(ix.str(head));
                }
            } else {
                push_dotted(ix, module, buf);
                if let Some(name) = st.name[row] {
                    if !module.is_empty() {
                        buf.push(b'.');
                    }
                    buf.extend_from_slice(ix.str(name));
                }
            }
        }
        _ => buf.extend_from_slice(ix.str(head)),
    }
    for &s in &chain[1..] {
        buf.push(b'.');
        buf.extend_from_slice(ix.str(s));
    }
}

/// Base-class entrypoint roots (see [`roots`]).
fn base_roots(
    ix: &Index,
    imp: Option<&Imports>,
    calls: &ResolvedCalls,
    mt: &EntrypointMatcher,
    opts: &RootOptions<'_>,
    set: &mut RootSet,
    buf: &mut Vec<u8>,
) {
    const SELF_NAME: u8 = 1;
    const WRITTEN_BASE: u8 = 2;
    let cl = calls.classes();
    let st = &ix.facts().bases;
    let mut hit = vec![0u8; cl.len() as usize];
    for c in 0..cl.len() {
        let v = cl.node(c);
        buf.clear();
        push_node_dotted(ix, v, buf);
        if mt.base(buf) {
            hit[c as usize] |= SELF_NAME;
        }
        let Some(f) = ix.file(v) else { continue };
        let view = ix.file_view(f);
        for r in view.bases(v - ix.file_node(f)) {
            let chain = view.base_chain(r);
            if st.keyword[r].is_some() || chain.is_empty() {
                continue;
            }
            buf.clear();
            push_expanded_base(ix, imp, f, chain, buf);
            if mt.base(buf) {
                hit[c as usize] |= WRITTEN_BASE;
                break;
            }
        }
    }
    for c in 0..cl.len() {
        let v = cl.node(c);
        if !ix.file(v).is_some_and(|f| opts.includes(ix.file_origin(f))) {
            continue;
        }
        let live = hit[c as usize] & WRITTEN_BASE != 0 || cl.mro(c).iter().skip(1).any(|&a| hit[a as usize] != 0);
        if live {
            set.add(v, root_kind::BASE);
            for ch in ix.children(v) {
                if ix.kind(ch) == NodeKind::Function {
                    set.add(ch, root_kind::BASE);
                }
            }
        }
    }
}
