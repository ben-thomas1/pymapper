//! Strict application of a TOML document onto a [`Config`].
//!
//! Each source (file or `--set` fragment) is parsed into toml's spanned document and walked by
//! hand: only keys present in the source are written, so sources layer key by key (arrays and
//! the `contracts` list are replaced whole). Every problem is recorded as a [`Diagnostic`] and the
//! walk continues, so one run reports all errors of a source.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use toml::Spanned;
use toml::de::{DeString, DeTable, DeValue};

use super::suggest::closest;
use super::{
    Config, Contract, ContractKind, ContractKindName, Diagnostic, Glob, ImportPattern, Mode, OutputFormat,
    PerFileIgnore, Preset, PythonVersion, RuleConfig, RuleSelector, normalize,
};

type Value<'i> = Spanned<DeValue<'i>>;
type Key<'i> = Spanned<DeString<'i>>;

pub(super) const ROOT_KEYS: &[&str] = &[
    "paths",
    "src",
    "exclude",
    "extend-exclude",
    "respect-gitignore",
    "include-deps",
    "include-stdlib",
    "venv",
    "python-version",
    "cache",
    "cache-dir",
    "threads",
    "mode",
    "output-format",
    "min-confidence",
    "baseline",
    "presets",
    "rules",
    "thresholds",
    "entrypoints",
    "analysis",
    "contracts",
];
pub(super) const RULES_KEYS: &[&str] = &["select", "extend-select", "ignore", "per-file-ignores"];
pub(super) const THRESHOLD_KEYS: &[&str] = &[
    "max-mccabe",
    "max-cognitive",
    "max-nesting",
    "max-function-loc",
    "max-params",
    "min-maintainability",
    "max-module-loc",
    "max-fan-out",
    "dupes-min-tokens",
    "dupes-min-lines",
];
pub(super) const ENTRYPOINT_KEYS: &[&str] = &["decorators", "bases", "names", "project-scripts"];
pub(super) const ANALYSIS_KEYS: &[&str] =
    &["dupes", "exact-reachability", "name-fallback", "unique-method-fallback", "type-checking-cycles", "coverage"];
pub(super) const CONTRACT_COMMON_KEYS: &[&str] = &["name", "kind", "allow-type-checking", "transitive", "ignore-imports"];
pub(super) const LAYERS_KEYS: &[&str] = &["layers", "containers", "independent-siblings"];
pub(super) const FORBIDDEN_KEYS: &[&str] = &["source", "forbidden"];
pub(super) const INDEPENDENCE_KEYS: &[&str] = &["modules"];

/// Every table and its keys (`""` is the root). Drives cross-table hints and the template test.
pub(super) const TABLES: &[(&str, &[&str])] = &[
    ("", ROOT_KEYS),
    ("rules", RULES_KEYS),
    ("thresholds", THRESHOLD_KEYS),
    ("entrypoints", ENTRYPOINT_KEYS),
    ("analysis", ANALYSIS_KEYS),
    ("contracts", CONTRACT_COMMON_KEYS),
    ("contracts", LAYERS_KEYS),
    ("contracts", FORBIDDEN_KEYS),
    ("contracts", INDEPENDENCE_KEYS),
];

/// One configuration source.
pub(super) struct Source<'a> {
    /// Shown in diagnostics (a path, or a description of a CLI source).
    pub origin: &'a str,
    /// The TOML text.
    pub text: &'a str,
    /// Directory relative paths are resolved against.
    pub base: &'a Path,
}

/// Applies a standalone document (top-level keys).
pub(super) fn apply_document(src: &Source<'_>, cfg: &mut Config, diags: &mut Vec<Diagnostic>) {
    let mut cx = Ctx { src, diags };
    if let Some(doc) = cx.parse() {
        apply_root(&mut cx, doc.get_ref(), cfg);
    }
    cx.finish();
}

/// Applies `[tool.pymapper]` of a `pyproject.toml`. Returns false if the table is absent (or the
/// file failed to parse, in which case a diagnostic was recorded).
pub(super) fn apply_pyproject(src: &Source<'_>, cfg: &mut Config, diags: &mut Vec<Diagnostic>) -> bool {
    let mut cx = Ctx { src, diags };
    let Some(doc) = cx.parse() else {
        cx.finish();
        return true;
    };
    let Some(value) = tool_pymapper(doc.get_ref()) else {
        return false;
    };
    if let Some(table) = cx.table("tool.pymapper", value) {
        apply_root(&mut cx, table, cfg);
    }
    cx.finish();
    true
}

/// `Some(has [tool.pymapper])`, or `None` if `text` is not valid TOML.
pub(super) fn pyproject_has_section(text: &str) -> Option<bool> {
    let doc = DeTable::parse(text).ok()?;
    Some(tool_pymapper(doc.get_ref()).is_some())
}

fn tool_pymapper<'a, 'i>(doc: &'a DeTable<'i>) -> Option<&'a Value<'i>> {
    let tool = get(doc, "tool")?;
    match tool.get_ref() {
        DeValue::Table(t) => get(t, "pymapper"),
        _ => None,
    }
}

fn get<'a, 'i>(t: &'a DeTable<'i>, key: &str) -> Option<&'a Value<'i>> {
    t.iter().find(|(k, _)| k.get_ref() == key).map(|(_, v)| v)
}

fn type_name(v: &DeValue<'_>) -> &'static str {
    match v {
        DeValue::String(_) => "a string",
        DeValue::Integer(_) => "an integer",
        DeValue::Float(_) => "a float",
        DeValue::Boolean(_) => "a boolean",
        DeValue::Datetime(_) => "a datetime",
        DeValue::Array(_) => "an array",
        DeValue::Table(_) => "a table",
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() { key.to_owned() } else { format!("{prefix}.{key}") }
}

struct Ctx<'s, 'd> {
    src: &'s Source<'s>,
    diags: &'d mut Vec<Diagnostic>,
}

impl<'s> Ctx<'s, '_> {
    fn parse(&mut self) -> Option<Spanned<DeTable<'s>>> {
        match DeTable::parse(self.src.text) {
            Ok(doc) => Some(doc),
            Err(e) => {
                let span = e.span().unwrap_or(0..0);
                self.error(span, format!("invalid TOML: {}", e.message()), None);
                None
            }
        }
    }

    /// Sorts this source's diagnostics by position (the document's maps iterate in key order).
    fn finish(&mut self) {
        // Diagnostics from earlier sources are already in order; only sort our tail. Line and
        // column order equals byte order within one source.
        let origin = self.src.origin;
        let start = self.diags.iter().rposition(|d| d.origin != origin).map_or(0, |i| i + 1);
        self.diags[start..].sort_by_key(|d| (d.line, d.col));
    }

    fn error(&mut self, span: Range<usize>, message: String, help: Option<String>) {
        let (line, col) = line_col(self.src.text, span.start);
        self.diags.push(Diagnostic { origin: self.src.origin.to_owned(), line, col, message, help });
    }

    fn unknown_key(&mut self, table: &str, key: &Key<'_>, known: &[&str]) {
        let name: &str = key.get_ref();
        let full = join(table, name);
        let help = closest(name, known.iter().copied())
            .map(|s| format!("did you mean `{}`?", join(table, s)))
            .or_else(|| {
                // Right key, wrong table?
                TABLES
                    .iter()
                    .find(|(t, keys)| *t != table && keys.contains(&name))
                    .map(|(t, _)| format!("`{name}` belongs in {}", if t.is_empty() { "the top-level table".to_owned() } else { format!("`[{t}]`") }))
            });
        self.error(key.span(), format!("unknown option `{full}`"), help);
    }

    fn type_error(&mut self, path: &str, v: &Value<'_>, expected: &str) {
        self.error(v.span(), format!("`{path}`: expected {expected}, found {}", type_name(v.get_ref())), None);
    }

    fn table<'v, 'i>(&mut self, path: &str, v: &'v Value<'i>) -> Option<&'v DeTable<'i>> {
        match v.get_ref() {
            DeValue::Table(t) => Some(t),
            _ => {
                self.type_error(path, v, "a table");
                None
            }
        }
    }

    fn bool(&mut self, path: &str, v: &Value<'_>) -> Option<bool> {
        match v.get_ref() {
            DeValue::Boolean(b) => Some(*b),
            _ => {
                self.type_error(path, v, "a boolean (`true` or `false`)");
                None
            }
        }
    }

    fn int(&mut self, path: &str, v: &Value<'_>, min: i64, max: i64) -> Option<i64> {
        let DeValue::Integer(i) = v.get_ref() else {
            self.type_error(path, v, "an integer");
            return None;
        };
        match i64::from_str_radix(i.as_str(), i.radix()) {
            Ok(n) if (min..=max).contains(&n) => Some(n),
            _ => {
                self.error(v.span(), format!("`{path}`: {i} is out of range ({min}..={max})"), None);
                None
            }
        }
    }

    fn u32(&mut self, path: &str, v: &Value<'_>, min: u32, max: u32) -> Option<u32> {
        // The range check guarantees the value fits.
        self.int(path, v, i64::from(min), i64::from(max)).map(|n| n as u32)
    }

    fn float(&mut self, path: &str, v: &Value<'_>, min: f64, max: f64) -> Option<f64> {
        let n = match v.get_ref() {
            DeValue::Float(f) => f.as_str().parse::<f64>().ok(),
            DeValue::Integer(i) => i64::from_str_radix(i.as_str(), i.radix()).ok().map(|n| n as f64),
            _ => {
                self.type_error(path, v, "a number");
                return None;
            }
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Some(n),
            _ => {
                self.error(v.span(), format!("`{path}`: value is out of range ({min}..={max})"), None);
                None
            }
        }
    }

    fn str<'v>(&mut self, path: &str, v: &'v Value<'_>) -> Option<&'v str> {
        match v.get_ref() {
            DeValue::String(s) => Some(s),
            _ => {
                self.type_error(path, v, "a string");
                None
            }
        }
    }

    fn nonempty_str<'v>(&mut self, path: &str, v: &'v Value<'_>) -> Option<&'v str> {
        let s = self.str(path, v)?;
        if s.trim().is_empty() {
            self.error(v.span(), format!("`{path}`: must not be empty"), None);
            return None;
        }
        Some(s)
    }

    fn array<'v, 'i>(&mut self, path: &str, v: &'v Value<'i>, expected: &str) -> Option<&'v [Value<'i>]> {
        match v.get_ref() {
            DeValue::Array(a) => Some(a),
            _ => {
                self.type_error(path, v, expected);
                None
            }
        }
    }

    /// Maps each array element through `f`; `None` if the value is not an array or any element
    /// failed (every failure is reported).
    fn list<'v, T>(
        &mut self,
        path: &str,
        v: &'v Value<'_>,
        expected: &str,
        mut f: impl FnMut(&mut Self, &str, &'v Value<'_>) -> Option<T>,
    ) -> Option<Vec<T>> {
        let items = self.array(path, v, expected)?;
        let mut out = Vec::with_capacity(items.len());
        let mut ok = true;
        for item in items {
            match f(self, path, item) {
                Some(x) => out.push(x),
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    fn string_list(&mut self, path: &str, v: &Value<'_>) -> Option<Vec<String>> {
        self.list(path, v, "an array of strings", |cx, p, item| cx.nonempty_str(p, item).map(str::to_owned))
    }

    fn path(&mut self, path: &str, v: &Value<'_>) -> Option<PathBuf> {
        let s = self.nonempty_str(path, v)?;
        Some(normalize(&self.src.base.join(s)))
    }

    fn path_list(&mut self, path: &str, v: &Value<'_>) -> Option<Vec<PathBuf>> {
        self.list(path, v, "an array of paths", |cx, p, item| cx.path(p, item))
    }

    fn glob_list(&mut self, path: &str, v: &Value<'_>) -> Option<Vec<String>> {
        self.list(path, v, "an array of glob patterns", |cx, p, item| {
            let s = cx.str(p, item)?;
            match Glob::new(s) {
                Ok(_) => Some(s.to_owned()),
                Err(e) => {
                    cx.error(item.span(), format!("`{p}`: invalid pattern `{s}`: {e}"), None);
                    None
                }
            }
        })
    }

    fn name<T: FromStr<Err = super::UnknownName>>(&mut self, path: &str, v: &Value<'_>) -> Option<T> {
        let s = self.str(path, v)?;
        match s.parse() {
            Ok(x) => Some(x),
            Err(e) => {
                let help = e.suggestion.map(|s| format!("did you mean `{s}`?"));
                self.error(v.span(), format!("`{path}`: {e}"), help);
                None
            }
        }
    }

    fn selector(&mut self, path: &str, v: &Value<'_>) -> Option<RuleSelector> {
        let s = self.str(path, v)?;
        match s.parse() {
            Ok(x) => Some(x),
            Err(e @ super::RuleSelectorError { .. }) => {
                let help = e.suggestion.as_ref().map(|s| format!("did you mean `{s}`?"));
                self.error(v.span(), format!("`{path}`: {e}"), help);
                None
            }
        }
    }

    fn selector_list(&mut self, path: &str, v: &Value<'_>) -> Option<Vec<RuleSelector>> {
        self.list(path, v, "an array of rule selectors", |cx, p, item| cx.selector(p, item))
    }

    fn module(&mut self, path: &str, v: &Value<'_>, wildcards: bool) -> Option<String> {
        let s = self.str(path, v)?;
        if valid_module(s, wildcards) {
            Some(s.to_owned())
        } else {
            self.error(v.span(), format!("`{path}`: invalid module name `{s}`"), Some(module_help(wildcards)));
            None
        }
    }

    fn module_list(&mut self, path: &str, v: &Value<'_>) -> Option<Vec<String>> {
        self.list(path, v, "an array of module names", |cx, p, item| cx.module(p, item, false))
    }
}

fn module_help(wildcards: bool) -> String {
    if wildcards {
        "use dotted names like `pkg.sub`; `*` matches one segment, `**` any number".to_owned()
    } else {
        "use dotted names like `pkg.sub` (wildcards are only allowed in `ignore-imports`)".to_owned()
    }
}

/// Dotted Python module name; with `wildcards`, a segment may also be `*` or `**`.
fn valid_module(s: &str, wildcards: bool) -> bool {
    !s.is_empty()
        && s.split('.').all(|seg| {
            if wildcards && (seg == "*" || seg == "**") {
                return true;
            }
            let mut chars = seg.chars();
            chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
                && chars.all(|c| c == '_' || c.is_alphanumeric())
        })
}

/// 1-based line and character column of byte offset `pos`.
fn line_col(text: &str, pos: usize) -> (u32, u32) {
    let pos = pos.min(text.len());
    let before = text.get(..pos).unwrap_or(text);
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = before[line_start..].chars().count() + 1;
    (u32::try_from(line).unwrap_or(u32::MAX), u32::try_from(col).unwrap_or(u32::MAX))
}

fn apply_root(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, cfg: &mut Config) {
    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        match key {
            "paths" => {
                if let Some(x) = cx.path_list(key, v) {
                    if x.is_empty() {
                        cx.error(v.span(), "`paths`: must list at least one path".to_owned(), None);
                    } else {
                        cfg.paths = x;
                    }
                }
            }
            "src" => set(&mut cfg.src, cx.path_list(key, v)),
            "exclude" => set(&mut cfg.exclude, cx.string_list(key, v)),
            "extend-exclude" => set(&mut cfg.extend_exclude, cx.string_list(key, v)),
            "respect-gitignore" => set(&mut cfg.respect_gitignore, cx.bool(key, v)),
            "include-deps" => set(&mut cfg.include_deps, cx.bool(key, v)),
            "include-stdlib" => set(&mut cfg.include_stdlib, cx.bool(key, v)),
            "venv" => set(&mut cfg.venv, cx.path(key, v).map(Some)),
            "python-version" => {
                if let Some(s) = cx.str(key, v) {
                    match s.parse::<PythonVersion>() {
                        Ok(pv) => cfg.python_version = Some(pv),
                        Err(e) => cx.error(v.span(), format!("`{key}`: {e}"), None),
                    }
                }
            }
            "cache" => set(&mut cfg.cache, cx.bool(key, v)),
            "cache-dir" => set(&mut cfg.cache_dir, cx.path(key, v)),
            "threads" => set(&mut cfg.threads, cx.u32(key, v, 0, 4096)),
            "mode" => set(&mut cfg.mode, cx.name::<Mode>(key, v)),
            "output-format" => set(&mut cfg.output_format, cx.name::<OutputFormat>(key, v)),
            "min-confidence" => set(&mut cfg.min_confidence, cx.u32(key, v, 0, 100).map(|n| n as u8)),
            "baseline" => set(&mut cfg.baseline, cx.path(key, v).map(Some)),
            "presets" => {
                if let Some(list) = cx.list(key, v, "an array of preset names", |cx, p, item| cx.name::<Preset>(p, item)) {
                    let mut presets = Vec::with_capacity(list.len());
                    for p in list {
                        if !presets.contains(&p) {
                            presets.push(p);
                        }
                    }
                    cfg.presets = presets;
                }
            }
            "rules" => {
                if let Some(t) = cx.table(key, v) {
                    apply_rules(cx, t, &mut cfg.rules);
                }
            }
            "thresholds" => {
                if let Some(t) = cx.table(key, v) {
                    apply_thresholds(cx, t, cfg);
                }
            }
            "entrypoints" => {
                if let Some(t) = cx.table(key, v) {
                    apply_entrypoints(cx, t, cfg);
                }
            }
            "analysis" => {
                if let Some(t) = cx.table(key, v) {
                    apply_analysis(cx, t, cfg);
                }
            }
            "contracts" => set(&mut cfg.contracts, contracts(cx, v)),
            _ => cx.unknown_key("", k, ROOT_KEYS),
        }
    }
}

#[inline]
fn set<T>(slot: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *slot = v;
    }
}

fn apply_rules(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, rules: &mut RuleConfig) {
    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        let path = join("rules", key);
        match key {
            "select" => set(&mut rules.select, cx.selector_list(&path, v).map(Some)),
            "extend-select" => set(&mut rules.extend_select, cx.selector_list(&path, v)),
            "ignore" => set(&mut rules.ignore, cx.selector_list(&path, v)),
            "per-file-ignores" => {
                let Some(map) = cx.table(&path, v) else { continue };
                let mut out = Vec::with_capacity(map.len());
                let mut ok = true;
                for (pk, pv) in map.iter() {
                    let pattern: &str = pk.get_ref();
                    if pattern.trim().is_empty() {
                        cx.error(pk.span(), format!("`{path}`: empty path pattern"), None);
                        ok = false;
                        continue;
                    }
                    let item_path = format!("{path}.\"{pattern}\"");
                    match cx.selector_list(&item_path, pv) {
                        Some(sel) => out.push(PerFileIgnore { pattern: pattern.to_owned(), rules: sel }),
                        None => ok = false,
                    }
                }
                if ok {
                    // The document map iterates in key order, so entries are sorted by pattern.
                    rules.per_file_ignores = out;
                }
            }
            _ => cx.unknown_key("rules", k, RULES_KEYS),
        }
    }
}

fn apply_thresholds(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, cfg: &mut Config) {
    let th = &mut cfg.thresholds;
    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        let path = join("thresholds", key);
        let max = u32::MAX >> 1;
        match key {
            "max-mccabe" => set(&mut th.max_mccabe, cx.u32(&path, v, 1, max)),
            "max-cognitive" => set(&mut th.max_cognitive, cx.u32(&path, v, 0, max)),
            "max-nesting" => set(&mut th.max_nesting, cx.u32(&path, v, 0, max)),
            "max-function-loc" => set(&mut th.max_function_loc, cx.u32(&path, v, 1, max)),
            "max-params" => set(&mut th.max_params, cx.u32(&path, v, 0, max)),
            "min-maintainability" => set(&mut th.min_maintainability, cx.float(&path, v, 0.0, 100.0)),
            "max-module-loc" => set(&mut th.max_module_loc, cx.u32(&path, v, 1, max)),
            "max-fan-out" => set(&mut th.max_fan_out, cx.u32(&path, v, 0, max)),
            "dupes-min-tokens" => set(&mut th.dupes_min_tokens, cx.u32(&path, v, 10, max)),
            "dupes-min-lines" => set(&mut th.dupes_min_lines, cx.u32(&path, v, 2, max)),
            _ => cx.unknown_key("thresholds", k, THRESHOLD_KEYS),
        }
    }
}

fn apply_entrypoints(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, cfg: &mut Config) {
    let ep = &mut cfg.entrypoints;
    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        let path = join("entrypoints", key);
        match key {
            "decorators" => set(&mut ep.decorators, cx.glob_list(&path, v)),
            "bases" => set(&mut ep.bases, cx.glob_list(&path, v)),
            "names" => set(&mut ep.names, cx.glob_list(&path, v)),
            "project-scripts" => set(&mut ep.project_scripts, cx.bool(&path, v)),
            _ => cx.unknown_key("entrypoints", k, ENTRYPOINT_KEYS),
        }
    }
}

fn apply_analysis(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, cfg: &mut Config) {
    let an = &mut cfg.analysis;
    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        let path = join("analysis", key);
        match key {
            "dupes" => set(&mut an.dupes, cx.bool(&path, v)),
            "exact-reachability" => set(&mut an.exact_reachability, cx.bool(&path, v)),
            "name-fallback" => set(&mut an.name_fallback, cx.bool(&path, v)),
            "unique-method-fallback" => set(&mut an.unique_method_fallback, cx.bool(&path, v)),
            "type-checking-cycles" => set(&mut an.type_checking_cycles, cx.bool(&path, v)),
            "coverage" => set(&mut an.coverage, cx.path(&path, v).map(Some)),
            _ => cx.unknown_key("analysis", k, ANALYSIS_KEYS),
        }
    }
}

fn contracts(cx: &mut Ctx<'_, '_>, v: &Value<'_>) -> Option<Vec<Contract>> {
    let list = cx.list("contracts", v, "an array of tables (`[[contracts]]`)", |cx, _, item| {
        let t = cx.table("contracts", item)?;
        contract(cx, t, item.span())
    })?;
    // Names must be unique: findings refer to contracts by name.
    let mut names: Vec<(&str, usize)> = list.iter().enumerate().map(|(i, c)| (c.name.as_str(), i)).collect();
    names.sort_unstable();
    let mut ok = true;
    for w in names.windows(2) {
        if w[0].0 == w[1].0 {
            let span = match v.get_ref() {
                DeValue::Array(a) => a[w[1].1].span(),
                _ => v.span(),
            };
            cx.error(span, format!("duplicate contract name `{}`", w[1].0), None);
            ok = false;
        }
    }
    ok.then_some(list)
}

fn contract(cx: &mut Ctx<'_, '_>, t: &DeTable<'_>, span: Range<usize>) -> Option<Contract> {
    let mut name = None;
    let mut kind = None;
    let mut ok = true;
    for (k, v) in t.iter() {
        match &**k.get_ref() {
            "name" => name = cx.nonempty_str("contracts.name", v).map(str::to_owned),
            "kind" => kind = cx.name::<ContractKindName>("contracts.kind", v),
            _ => {}
        }
    }
    let label = name.as_deref().unwrap_or("<unnamed>").to_owned();
    if !t.iter().any(|(k, _)| k.get_ref() == "name") {
        cx.error(span.clone(), "contract is missing `name`".to_owned(), None);
        ok = false;
    }
    if !t.iter().any(|(k, _)| k.get_ref() == "kind") {
        let help = format!("add `kind = ` one of: {}", ContractKindName::NAMES.join(", "));
        cx.error(span.clone(), format!("contract `{label}` is missing `kind`"), Some(help));
        ok = false;
    }

    let mut allow_type_checking = true;
    let mut transitive = false;
    let mut ignore_imports = Vec::new();
    let mut layers: Option<Vec<Vec<String>>> = None;
    let mut containers = Vec::new();
    let mut independent_siblings = false;
    let mut source: Option<Vec<String>> = None;
    let mut forbidden: Option<Vec<String>> = None;
    let mut modules: Option<Vec<String>> = None;

    for (k, v) in t.iter() {
        let key: &str = k.get_ref();
        let path = join("contracts", key);
        let specific = [
            (ContractKindName::Layers, LAYERS_KEYS),
            (ContractKindName::Forbidden, FORBIDDEN_KEYS),
            (ContractKindName::Independence, INDEPENDENCE_KEYS),
        ];
        if let (Some(kind), Some((owner, _))) = (kind, specific.iter().find(|(_, keys)| keys.contains(&key)))
            && kind != *owner
        {
            cx.error(
                k.span(),
                format!("`{path}` is not valid for contracts of kind `{kind}`"),
                Some(format!("`{key}` belongs to kind `{owner}`")),
            );
            ok = false;
            continue;
        }
        let parsed = match key {
            "name" | "kind" => true,
            "allow-type-checking" => cx.bool(&path, v).map(|b| allow_type_checking = b).is_some(),
            "transitive" => cx.bool(&path, v).map(|b| transitive = b).is_some(),
            "ignore-imports" => cx
                .list(&path, v, "an array of `importer -> imported` strings", |cx, p, item| import_pattern(cx, p, item))
                .map(|x| ignore_imports = x)
                .is_some(),
            "layers" => cx
                .list(&path, v, "an array of layers", |cx, p, item| match item.get_ref() {
                    DeValue::String(_) => cx.module(p, item, false).map(|m| vec![m]),
                    DeValue::Array(_) => {
                        let siblings = cx.module_list(p, item)?;
                        if siblings.is_empty() {
                            cx.error(item.span(), format!("`{p}`: a layer must name at least one module"), None);
                            return None;
                        }
                        Some(siblings)
                    }
                    _ => {
                        cx.type_error(p, item, "a module name or an array of sibling module names");
                        None
                    }
                })
                .map(|x| layers = Some(x))
                .is_some(),
            "containers" => cx.module_list(&path, v).map(|x| containers = x).is_some(),
            "independent-siblings" => cx.bool(&path, v).map(|b| independent_siblings = b).is_some(),
            "source" => cx.module_list(&path, v).map(|x| source = Some(x)).is_some(),
            "forbidden" => cx.module_list(&path, v).map(|x| forbidden = Some(x)).is_some(),
            "modules" => cx.module_list(&path, v).map(|x| modules = Some(x)).is_some(),
            _ => {
                let mut known: Vec<&str> = CONTRACT_COMMON_KEYS.to_vec();
                known.extend_from_slice(match kind {
                    Some(ContractKindName::Layers) => LAYERS_KEYS,
                    Some(ContractKindName::Forbidden) => FORBIDDEN_KEYS,
                    Some(ContractKindName::Independence) => INDEPENDENCE_KEYS,
                    None => &[],
                });
                cx.unknown_key("contracts", k, &known);
                false
            }
        };
        ok &= parsed;
    }

    let kind = kind?;
    // Only report a missing field if the key is absent; a present but invalid one has already
    // been reported.
    let mut need = |cx: &mut Ctx<'_, '_>, field: &str, parsed: bool| {
        if !parsed && !t.iter().any(|(k, _)| k.get_ref() == field) {
            cx.error(span.clone(), format!("contract `{label}` (kind `{kind}`) is missing `{field}`"), None);
            ok = false;
        }
    };
    let built = match kind {
        ContractKindName::Layers => {
            need(cx, "layers", layers.is_some());
            let layers = layers.unwrap_or_default();
            if layers.len() == 1 && ok {
                cx.error(span.clone(), format!("contract `{label}`: `layers` needs at least two layers"), None);
                ok = false;
            }
            ContractKind::Layers { layers, containers, independent_siblings }
        }
        ContractKindName::Forbidden => {
            need(cx, "source", source.is_some());
            need(cx, "forbidden", forbidden.is_some());
            ContractKind::Forbidden { source: source.unwrap_or_default(), forbidden: forbidden.unwrap_or_default() }
        }
        ContractKindName::Independence => {
            need(cx, "modules", modules.is_some());
            let modules = modules.unwrap_or_default();
            if modules.len() == 1 && ok {
                cx.error(span.clone(), format!("contract `{label}`: `modules` needs at least two modules"), None);
                ok = false;
            }
            ContractKind::Independence { modules }
        }
    };
    if ok && let Some(dup) = duplicate_module(&built) {
        cx.error(span, format!("contract `{label}`: module `{dup}` is listed more than once"), None);
        ok = false;
    }
    let name = name?;
    ok.then_some(Contract { name, kind: built, allow_type_checking, transitive, ignore_imports })
}

/// A module named twice within one contract role set (ambiguous layering / no-op rule).
fn duplicate_module(kind: &ContractKind) -> Option<String> {
    let mut all: Vec<&str> = match kind {
        ContractKind::Layers { layers, .. } => layers.iter().flatten().map(String::as_str).collect(),
        ContractKind::Forbidden { source, forbidden } => source.iter().chain(forbidden).map(String::as_str).collect(),
        ContractKind::Independence { modules } => modules.iter().map(String::as_str).collect(),
    };
    all.sort_unstable();
    all.windows(2).find(|w| w[0] == w[1]).map(|w| w[0].to_owned())
}

fn import_pattern(cx: &mut Ctx<'_, '_>, path: &str, v: &Value<'_>) -> Option<ImportPattern> {
    let s = cx.str(path, v)?;
    let help = || Some("write `importer -> imported`, e.g. `pkg.api -> pkg.db.*`".to_owned());
    let Some((a, b)) = s.split_once("->") else {
        cx.error(v.span(), format!("`{path}`: expected `importer -> imported`, found `{s}`"), help());
        return None;
    };
    let (a, b) = (a.trim(), b.trim());
    for side in [a, b] {
        if !valid_module(side, true) {
            cx.error(v.span(), format!("`{path}`: invalid module pattern `{side}` in `{s}`"), Some(module_help(true)));
            return None;
        }
    }
    Some(ImportPattern { importer: a.to_owned(), imported: b.to_owned() })
}
