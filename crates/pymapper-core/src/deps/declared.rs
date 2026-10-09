//! Declared dependencies of the project units, from their `pyproject.toml` (PM601, PM602, PM604).
//!
//! ```text
//!   unit u ──> <dir>/pyproject.toml ──> [project] dependencies                  Main
//!                                       [project] optional-dependencies         Optional
//!                                       [dependency-groups] (PEP 735)           Group
//!                                       [tool.poetry] dependencies              Main (optional = true: Optional)
//!                                       [tool.poetry] dev-dependencies, group.*.dependencies   Group
//!                                       [tool.uv] dev-dependencies, [tool.pdm] dev-dependencies Group
//!                                       `# pymapper: ignore[...]` comments      suppressions by line
//! ```
//!
//! Requirement strings are PEP 508: only the name and the extras matter (`requests[socks]>=2`
//! declares `requests` with extra `socks`). Names are PEP 503 normalised and interned in the
//! caller's [`Interner`] (the hygiene names, rendered by [`super::hygiene::HygieneContext`]).
//!
//! A unit that declares nothing (no `[project]` table and no poetry dependencies) inherits the
//! declarations of the nearest enclosing unit that does ([`Declarations::effective`]); one whose
//! `[project] dynamic` lists `dependencies` has unknown dependencies, so PM601, PM602 and PM604
//! are not reported for it. Cost: one TOML parse per unit, a few allocations per dependency.

use std::path::{Path, PathBuf};

use toml::de::{DeTable, DeValue};

use crate::config::PYPROJECT_FILE;
use crate::index::NONE;
use crate::intern::{FxHashMap, Interner, Sym};
use crate::roots::{ProjectUnits, normalize_name};
use crate::rules::{self, BadCodeKind, FileId, SuppressionEntry};

/// Where a dependency is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DepKind {
    /// `[project] dependencies` / poetry main dependencies: required at runtime (PM602 checks
    /// these only).
    Main,
    /// `[project] optional-dependencies` (extras) / poetry `optional = true`.
    Optional,
    /// A development group: PEP 735 `[dependency-groups]`, poetry groups, uv / pdm dev
    /// dependencies.
    Group,
}

/// One declared requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeclaredDep {
    /// The name as written (`PyYAML`).
    pub name: Sym,
    /// PEP 503 normalised name (`pyyaml`).
    pub norm: Sym,
    /// Requested extras (normalised): `extras[start..start + len]` of [`Declarations::extras`].
    pub extras_start: u32,
    pub extras_len: u32,
    pub kind: DepKind,
    /// 1-based line and byte column of the requirement in `pyproject.toml`.
    pub line: u32,
    pub col: u32,
}

/// What a unit's `pyproject.toml` says about its dependencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclStatus {
    /// No `pyproject.toml`, an unreadable or invalid one, or one without `[project]` and
    /// poetry dependencies: the unit inherits its parent's declarations.
    None,
    /// The dependencies are listed.
    Declared,
    /// `[project] dynamic` lists `dependencies`: unknown (nothing is reported).
    Dynamic,
}

/// The declarations of one unit.
#[derive(Clone, Debug)]
pub struct UnitDecl {
    pub path: PathBuf,
    pub status: DeclStatus,
    /// `[project] dynamic` lists `optional-dependencies`: imports of undeclared distributions
    /// may be extras (PM601 / PM604 are not reported).
    pub dynamic_optional: bool,
    /// `[project] name` (or poetry `name`), normalised.
    pub project: Option<Sym>,
    /// Dependencies: `deps[start..start + len]` of [`Declarations::deps`].
    deps_start: u32,
    deps_len: u32,
    /// Inline suppressions found in the file's comments, sorted by line.
    pub suppressions: Vec<SuppressionEntry>,
    /// Malformed or unknown suppression codes: `(line, code, kind)`, for warnings.
    pub bad_suppressions: Vec<(u32, Box<[u8]>, BadCodeKind)>,
    /// Why the file could not be used (unreadable, invalid TOML), for a warning.
    pub error: Option<String>,
}

/// Declared dependencies of every unit of a project.
#[derive(Clone, Debug, Default)]
pub struct Declarations {
    units: Vec<UnitDecl>,
    deps: Vec<DeclaredDep>,
    extras: Vec<Sym>,
    /// Per unit: the unit whose declarations apply to its files (`NONE`: none does).
    effective: Vec<u32>,
    /// `(unit, normalised name)` -> index of its first declaration in `deps`.
    by_name: FxHashMap<(u32, Sym), u32>,
    /// [`FileId`] of unit 0's `pyproject.toml`; unit `u`'s is `file_base + u`.
    file_base: u32,
}

impl Declarations {
    /// Reads the `pyproject.toml` of every unit (one read and parse each). `file_base` is the
    /// first [`FileId`] free for them (the index's file count), so findings in `pyproject.toml`
    /// files get ids that collide with no Python file.
    pub fn load(units: &ProjectUnits, file_base: u32, names: &mut Interner) -> Declarations {
        let mut texts = Vec::with_capacity(units.len() as usize);
        for u in 0..units.len() {
            let path = units.dir(u).join(PYPROJECT_FILE);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => Ok(Some(t)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            };
            texts.push((path, text));
        }
        let parents: Vec<u32> = (0..units.len()).map(|u| parent_unit(units, u)).collect();
        Declarations::from_texts(texts, &parents, file_base, names)
    }

    /// [`load`](Self::load) over already read files: per unit its path and its text (`Ok(None)`:
    /// no file, `Err`: unreadable), and its enclosing unit (`NONE` for the root).
    pub fn from_texts(
        texts: Vec<(PathBuf, Result<Option<String>, String>)>,
        parents: &[u32],
        file_base: u32,
        names: &mut Interner,
    ) -> Declarations {
        let mut d = Declarations { file_base, ..Declarations::default() };
        let mut scratch = Vec::new();
        for (u, (path, text)) in texts.into_iter().enumerate() {
            let deps_start = d.deps.len() as u32;
            let mut unit = UnitDecl {
                path,
                status: DeclStatus::None,
                dynamic_optional: false,
                project: None,
                deps_start,
                deps_len: 0,
                suppressions: Vec::new(),
                bad_suppressions: Vec::new(),
                error: None,
            };
            match text {
                Ok(Some(text)) => {
                    let mut p = Parser { names, deps: &mut d.deps, extras: &mut d.extras, lines: LineIndex::new(&text), scratch: &mut scratch };
                    match p.parse(&text, &mut unit) {
                        Ok(()) => {
                            let bad = &mut unit.bad_suppressions;
                            toml_suppressions(text.as_bytes(), &mut unit.suppressions, |line, code, kind| bad.push((line, code.into(), kind)));
                        }
                        Err(e) => unit.error = Some(e),
                    }
                }
                Ok(None) => {}
                Err(e) => unit.error = Some(e),
            }
            unit.deps_len = d.deps.len() as u32 - deps_start;
            for (i, dep) in d.deps[deps_start as usize..].iter().enumerate() {
                d.by_name.entry((u as u32, dep.norm)).or_insert(deps_start + i as u32);
            }
            d.units.push(unit);
        }
        // Units come in discovery order, which is not parents first: resolve by walking up.
        for u in 0..d.units.len() {
            let mut cur = u as u32;
            let mut eff = NONE;
            for _ in 0..d.units.len() {
                if cur == NONE {
                    break;
                }
                if d.units[cur as usize].status != DeclStatus::None {
                    eff = cur;
                    break;
                }
                cur = parents.get(cur as usize).copied().unwrap_or(NONE);
            }
            d.effective.push(eff);
        }
        d
    }

    /// Number of units.
    pub fn len(&self) -> u32 {
        self.units.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Declarations of unit `u`.
    pub fn unit(&self, u: u32) -> &UnitDecl {
        &self.units[u as usize]
    }

    /// The dependencies declared by unit `u`, in file order.
    pub fn deps(&self, u: u32) -> &[DeclaredDep] {
        let d = &self.units[u as usize];
        &self.deps[d.deps_start as usize..(d.deps_start + d.deps_len) as usize]
    }

    /// Extras requested by `dep`.
    pub fn extras(&self, dep: &DeclaredDep) -> &[Sym] {
        &self.extras[dep.extras_start as usize..(dep.extras_start + dep.extras_len) as usize]
    }

    /// The unit whose declarations apply to the files of unit `u` (itself, or the nearest
    /// enclosing unit that declares dependencies); `NONE` when none does.
    #[inline]
    pub fn effective(&self, u: u32) -> u32 {
        self.effective.get(u as usize).copied().unwrap_or(NONE)
    }

    /// First declaration of the normalised name `norm` by unit `u`, in any group. O(1).
    #[inline]
    pub fn find(&self, u: u32, norm: Sym) -> Option<&DeclaredDep> {
        self.by_name.get(&(u, norm)).map(|&i| &self.deps[i as usize])
    }

    /// The [`FileId`] given to unit `u`'s `pyproject.toml`.
    #[inline]
    pub fn file_id(&self, u: u32) -> FileId {
        FileId(self.file_base + u)
    }

    /// The unit whose `pyproject.toml` has id `f`, if it is one.
    #[inline]
    pub fn unit_of_file(&self, f: FileId) -> Option<u32> {
        let u = f.0.checked_sub(self.file_base)?;
        ((u as usize) < self.units.len()).then_some(u)
    }
}

/// The nearest unit strictly enclosing unit `u` (`NONE` for the root).
fn parent_unit(units: &ProjectUnits, u: u32) -> u32 {
    if u == 0 {
        return NONE;
    }
    match units.dir(u).parent() {
        Some(p) => units.unit_of_dir(p.as_os_str().as_encoded_bytes()),
        None => 0,
    }
}

/// Byte offset -> (line, column), both 1-based.
struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    fn new(text: &str) -> LineIndex {
        let mut starts = vec![0u32];
        starts.extend(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i as u32 + 1));
        LineIndex { starts }
    }

    fn pos(&self, off: usize) -> (u32, u32) {
        let off = off as u32;
        let line = self.starts.partition_point(|&s| s <= off);
        (line as u32, off - self.starts[line - 1] + 1)
    }
}

type Table<'i> = DeTable<'i>;
type Spanned<T> = toml::Spanned<T>;

fn get<'a, 'i>(t: &'a Table<'i>, key: &str) -> Option<&'a Spanned<DeValue<'i>>> {
    t.iter().find(|(k, _)| k.get_ref() == key).map(|(_, v)| v)
}

fn table<'a, 'i>(t: &'a Table<'i>, path: &[&str]) -> Option<&'a Table<'i>> {
    let mut cur = t;
    for key in path {
        match get(cur, key)?.get_ref() {
            DeValue::Table(next) => cur = next,
            _ => return None,
        }
    }
    Some(cur)
}

fn as_str<'a>(v: &'a DeValue<'_>) -> Option<&'a str> {
    match v {
        DeValue::String(s) => Some(s),
        _ => None,
    }
}

/// One `pyproject.toml` being parsed into the shared tables.
struct Parser<'p> {
    names: &'p mut Interner,
    deps: &'p mut Vec<DeclaredDep>,
    extras: &'p mut Vec<Sym>,
    lines: LineIndex,
    scratch: &'p mut Vec<u8>,
}

impl Parser<'_> {
    fn parse(&mut self, text: &str, unit: &mut UnitDecl) -> Result<(), String> {
        let doc = DeTable::parse(text).map_err(|e| format!("invalid TOML: {}", e.message()))?;
        let doc = doc.get_ref();
        let mut declares = false;
        if let Some(project) = table(doc, &["project"]) {
            declares = true;
            if let Some(name) = get(project, "name").and_then(|v| as_str(v.get_ref())) {
                unit.project = Some(self.norm(name.as_bytes()));
            }
            if let Some(v) = get(project, "dynamic")
                && let DeValue::Array(a) = v.get_ref()
            {
                for item in a {
                    match as_str(item.get_ref()) {
                        Some("dependencies") => unit.status = DeclStatus::Dynamic,
                        Some("optional-dependencies") => unit.dynamic_optional = true,
                        _ => {}
                    }
                }
            }
            if let Some(v) = get(project, "dependencies") {
                self.array(v, DepKind::Main);
            }
            if let Some(v) = get(project, "optional-dependencies")
                && let DeValue::Table(extras) = v.get_ref()
            {
                for (_, group) in extras.iter() {
                    self.array(group, DepKind::Optional);
                }
            }
        }
        if let Some(groups) = table(doc, &["dependency-groups"]) {
            for (_, group) in groups.iter() {
                // `{include-group = "..."}` entries are tables: the included group is read too.
                self.array(group, DepKind::Group);
            }
        }
        if let Some(v) = table(doc, &["tool", "uv"]).and_then(|uv| get(uv, "dev-dependencies")) {
            self.array(v, DepKind::Group);
        }
        if let Some(pdm) = table(doc, &["tool", "pdm", "dev-dependencies"]) {
            for (_, group) in pdm.iter() {
                self.array(group, DepKind::Group);
            }
        }
        if let Some(poetry) = table(doc, &["tool", "poetry"]) {
            if unit.project.is_none()
                && let Some(name) = get(poetry, "name").and_then(|v| as_str(v.get_ref()))
            {
                unit.project = Some(self.norm(name.as_bytes()));
            }
            if let Some(main) = table(poetry, &["dependencies"]) {
                declares = true;
                self.poetry(main, DepKind::Main);
            }
            if let Some(dev) = table(poetry, &["dev-dependencies"]) {
                self.poetry(dev, DepKind::Group);
            }
            if let Some(groups) = table(poetry, &["group"]) {
                for (_, g) in groups.iter() {
                    if let DeValue::Table(g) = g.get_ref()
                        && let Some(deps) = table(g, &["dependencies"])
                    {
                        self.poetry(deps, DepKind::Group);
                    }
                }
            }
        }
        if declares && unit.status == DeclStatus::None {
            unit.status = DeclStatus::Declared;
        }
        Ok(())
    }

    fn norm(&mut self, name: &[u8]) -> Sym {
        normalize_name(name, self.scratch);
        self.names.intern(self.scratch)
    }

    /// The PEP 508 requirement strings of an array value.
    fn array(&mut self, v: &Spanned<DeValue<'_>>, kind: DepKind) {
        let DeValue::Array(items) = v.get_ref() else { return };
        for item in items {
            if let Some(s) = as_str(item.get_ref()) {
                self.requirement(s, item.span().start, kind);
            }
        }
    }

    /// Poetry's `name = "constraint"` / `name = { version = ..., optional = true }` tables.
    fn poetry(&mut self, t: &Table<'_>, kind: DepKind) {
        for (k, v) in t.iter() {
            let name: &str = k.get_ref();
            if name.eq_ignore_ascii_case("python") {
                continue;
            }
            let mut kind = kind;
            let mut extras: &[Spanned<DeValue<'_>>] = &[];
            if let DeValue::Table(spec) = v.get_ref() {
                if kind == DepKind::Main && get(spec, "optional").is_some_and(|o| matches!(o.get_ref(), DeValue::Boolean(true))) {
                    kind = DepKind::Optional;
                }
                if let Some(e) = get(spec, "extras")
                    && let DeValue::Array(a) = e.get_ref()
                {
                    extras = a;
                }
            }
            let extras_start = self.extras.len() as u32;
            for e in extras {
                if let Some(e) = as_str(e.get_ref()) {
                    let s = self.norm(e.as_bytes());
                    self.extras.push(s);
                }
            }
            self.push(name, extras_start, k.span().start, kind);
        }
    }

    fn requirement(&mut self, req: &str, off: usize, kind: DepKind) {
        let Some((name, extras)) = parse_requirement(req) else { return };
        let extras_start = self.extras.len() as u32;
        for e in extras.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let s = self.norm(e.as_bytes());
            self.extras.push(s);
        }
        self.push(name, extras_start, off, kind);
    }

    fn push(&mut self, name: &str, extras_start: u32, off: usize, kind: DepKind) {
        let norm = self.norm(name.as_bytes());
        let name = self.names.intern(name.as_bytes());
        let (line, col) = self.lines.pos(off);
        let extras_len = self.extras.len() as u32 - extras_start;
        self.deps.push(DeclaredDep { name, norm, extras_start, extras_len, kind, line, col });
    }
}

/// Name and extras list (the text between `[` and `]`, possibly empty) of a PEP 508
/// requirement (`name [extras] (version) ; marker`, or `name @ url`). `None` when it does not
/// start with a valid project name.
pub fn parse_requirement(req: &str) -> Option<(&str, &str)> {
    let s = req.trim_start();
    let end = s.bytes().position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))).unwrap_or(s.len());
    let name = &s[..end];
    let (first, last) = (*name.as_bytes().first()?, *name.as_bytes().last()?);
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return None;
    }
    let rest = s[end..].trim_start();
    let extras = match rest.strip_prefix('[') {
        Some(r) => &r[..r.find(']')?],
        None => "",
    };
    Some((name, extras))
}

/// Inline suppressions in the comments of a TOML document (`# pymapper: ignore[PM602]` after a
/// requirement), appended to `out` in line order. Comments are found with a small TOML string
/// state machine (basic, literal and multi-line strings), so a `#` inside a string is not one.
/// `bad(line, code, kind)` is called for each malformed or unknown code.
pub fn toml_suppressions(text: &[u8], out: &mut Vec<SuppressionEntry>, mut bad: impl FnMut(u32, &[u8], BadCodeKind)) {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Normal,
        Basic,
        Literal,
        MlBasic,
        MlLiteral,
    }
    let mut st = St::Normal;
    let mut found = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        if b == b'\n' {
            line += 1;
            // Single-line strings cannot span lines; recover from a malformed one.
            if matches!(st, St::Basic | St::Literal) {
                st = St::Normal;
            }
            i += 1;
            continue;
        }
        match st {
            St::Normal => match b {
                b'#' => {
                    let end = text[i..].iter().position(|&c| c == b'\n').map_or(text.len(), |n| i + n);
                    found.clear();
                    crate::lex::scan_suppressions(text, i, end, &mut found);
                    for s in &found {
                        if s.noqa {
                            continue;
                        }
                        if s.all {
                            rules::parse_codes(line, None, out, |_| {});
                            continue;
                        }
                        let codes = &text[s.codes_start as usize..(s.codes_start + s.codes_len) as usize];
                        rules::parse_codes(line, Some(codes), out, |e| {
                            let c = &codes[e.start as usize..(e.start + e.len) as usize];
                            bad(line, c, e.kind);
                        });
                    }
                    i = end;
                    continue;
                }
                b'"' if text[i..].starts_with(b"\"\"\"") => {
                    st = St::MlBasic;
                    i += 3;
                    continue;
                }
                b'\'' if text[i..].starts_with(b"'''") => {
                    st = St::MlLiteral;
                    i += 3;
                    continue;
                }
                b'"' => st = St::Basic,
                b'\'' => st = St::Literal,
                _ => {}
            },
            // Skip the escaped byte (a line-ending backslash in a multi-line string still
            // lets the newline count).
            St::Basic | St::MlBasic if b == b'\\' && text.get(i + 1) != Some(&b'\n') => i += 1,
            St::Basic if b == b'"' => st = St::Normal,
            St::Literal if b == b'\'' => st = St::Normal,
            St::MlBasic | St::MlLiteral => {
                let q = if st == St::MlBasic { b'"' } else { b'\'' };
                if b == q && text[i..].starts_with(&[q, q, q]) {
                    // Up to two quotes of content may precede the closing delimiter.
                    let n = text[i..].iter().take_while(|&&c| c == q).count().min(5);
                    st = St::Normal;
                    i += n;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// `path` relative to `root` when below it, else as is (for display).
pub fn display_path<'p>(path: &'p Path, root: &Path) -> &'p Path {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel,
        _ => path,
    }
}
