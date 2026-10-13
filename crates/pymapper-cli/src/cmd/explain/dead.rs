//! `explain` for dead code (PM401-404) and unused imports (PM405): the definition, the
//! evidence behind the confidence, what deleting it affects, the blind spots of this
//! repository and a verification command.

use std::path::Path;

use pymapper_core::index::{NodeId, NodeKind};
use pymapper_core::reach::root_kind;
use pymapper_core::resolve::Confidence;
use pymapper_core::roots::Origin;
use pymapper_core::rules::dead_code::reason;
use pymapper_core::rules::{Finding, reason_names};

use super::doc::{Doc, Sources, word_lines};
use super::{Cx, qid_arg, shell_word};
use crate::cmd::query::{Pair, locate};

/// Name-appearance lines listed at most (the rest are counted).
const MAX_APPEARANCES: usize = 12;
/// Callers, importers and chain members listed at most.
const MAX_ROWS: usize = 15;

/// What a confidence reason means (the names `check` prints).
pub fn reason_text(name: &str) -> &'static str {
    match name {
        "no-references" => "nothing refers to it at all",
        "only-used-by-unused-code" => "its only references come from code that is itself unused",
        "name-in-same-file" => "its name occurs elsewhere in its own file (not as a resolved call)",
        "name-in-importer" => "its name occurs in a file that imports it or its module",
        "name-elsewhere" => "its name occurs in some other file (attribute, identifier, string, config)",
        "possible-call-only" => "reached only through `possible` calls (a guess from a unique method name)",
        "module-dynamic-access" => "its module uses dynamic access (getattr with a variable, globals(), __getattr__, ...)",
        "dynamic-base" => "a loaded base class's module uses dynamic access (name-based dispatch)",
        "getattr-prefix" => "its name starts with the constant prefix of a dynamic getattr",
        "unknown-decorator" => "it carries a decorator that may register it",
        "external-base" => "a method of a class whose base is not loaded (may override a framework method)",
        "exported" => "listed in __all__ or re-exported (application mode)",
        "library-public-not-in-all" => "a public name outside the public API (library mode)",
        "library-api-method" => "a public method of a class outside the public API (library mode)",
        "config-reference" => "named in a config file (YAML, TOML, JSON, ...)",
        "string-reference" => "its dotted path occurs in a string literal",
        "script-like" => "a module outside any regular package (may be run as a script)",
        "star-imported" => "another module star-imports this file",
        "init-reexport" => "an __init__.py re-export (application mode)",
        "package-init" => "in an __init__.py",
        "init-submodule" => "a submodule import in an __init__.py (often for side effects)",
        "type-checking-import" => "under `if TYPE_CHECKING:`",
        "conditional-import" => "a conditional import",
        "external-override" => "overrides a method of a third-party or stdlib base class",
        "used-only-in-tests" => "its name occurs only in test code (test-only API)",
        "never-executed" => "the coverage report shows its code never ran",
        _ => "",
    }
}

/// The bare name of `v` (its qualified id's last segment, without a `#N` suffix).
pub fn bare_name(cx: &Cx<'_>, v: NodeId) -> String {
    let id = cx.ix.qid_string(v);
    let tail = id.rsplit([':', '.']).next().unwrap_or(&id);
    tail.split('#').next().unwrap_or(tail).to_owned()
}

/// Lines `(first, name, last)` of a class or function's definition (decorators included).
fn def_lines(cx: &Cx<'_>, v: NodeId) -> Option<(u32, u32, u32)> {
    let r = cx.ix.def_row(v).filter(|_| cx.ix.kind(v).is_def())?;
    let d = &cx.ix.facts().defs;
    Some((d.start_line[r], d.name_line[r], d.end_line[r]))
}

/// The dead-code explanation of the finding at `pos`.
pub fn explain(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, pos: usize) {
    let f = cx.items[pos];
    let Some(v) = Cx::node(f) else { return };
    let ix = cx.ix;
    let module = ix.kind(v) == NodeKind::Module || ix.kind(v) == NodeKind::Package;
    let path = ix.file(v).map(|fl| ix.file_path(fl).to_path_buf());
    // The definition.
    if let Some(path) = &path {
        let (first, mark, last) = def_lines(cx, v).unwrap_or((1, 1, 12));
        let shown = last.min(first + 11);
        doc.section(&format!("Source {}:{}-{}", doc.rel(path), first, last));
        match sources.get(path) {
            Some(src) => {
                let last = if module { last.min(src.len()) } else { last };
                doc.quote(src, first, shown.min(last), &[mark]);
                if shown < last {
                    doc.line(&format!("... {} more lines", last - shown));
                }
            }
            None => doc.line("(cannot read the file)"),
        }
    }
    doc.section("Evidence");
    for name in reason_names(f) {
        let text = reason_text(name);
        doc.line(&if text.is_empty() { name.to_owned() } else { format!("{name}: {text}") });
    }
    if let Some(dc) = cx.dead {
        let (level, why) = if module {
            (dc.module_level(v).unwrap_or(0), dc.module_why(v).map_or("dead", |w| w.as_str()))
        } else {
            (dc.level(v), dc.why(v).as_str())
        };
        let why = if why == "dead" { "no path from any root" } else { why };
        doc.line(&format!("liveness level {level}/100 (confidence = 100 - level), from: {why}"));
    }
    if module {
        importers(cx, doc, v);
    } else {
        callers(cx, doc, v);
    }
    let name = bare_name(cx, v);
    appearances(cx, doc, sources, v, &name);
    if f.reasons & (reason::MODULE_DYNAMIC_ACCESS | reason::GETATTR_PREFIX | reason::DYNAMIC_BASE) != 0
        && let Some(path) = &path
    {
        dynamic_access(doc, sources, path);
    }
    if let (Some(path), Some((first, name_line, _))) = (&path, def_lines(cx, v))
        && let Some(src) = sources.get(path)
    {
        let decorators: Vec<&str> = (first..name_line).map(|n| src.line(n).trim()).filter(|l| l.starts_with('@')).collect();
        doc.line(&format!("decorators: {}", if decorators.is_empty() { "none".to_owned() } else { decorators.join(" ") }));
        if ix.kind(v) == NodeKind::Class {
            let header = src.line(name_line).trim();
            let bases = header.find('(').map_or("none", |i| header[i..].trim_end_matches(':'));
            doc.line(&format!("bases: {bases}"));
        }
    }
    roots(cx, doc, v);
    effect(cx, doc, v, module);
    blind_spots(cx, doc);
    doc.section("Verify");
    let root = cx.root_arg(doc);
    doc.line(&format!(
        "grep -rnw {} {root} --exclude-dir=.venv --exclude-dir=.git --exclude-dir=.pymapper_cache",
        shell_word(&name)
    ));
    if module {
        doc.line(&format!("grep -rn {} {root} --exclude-dir=.venv  (dotted path in strings or config)", shell_word(&ix.qid_string(v))));
    } else {
        doc.line(&format!("pymapper callers {}", qid_arg(ix, v)));
    }
}

/// Direct callers of `v`, each with its confidence, site and own liveness.
fn callers(cx: &Cx<'_>, doc: &mut Doc, v: NodeId) {
    let list: Vec<(NodeId, Confidence, bool)> = cx.q.direct_callers(v).collect();
    if list.is_empty() {
        doc.line("call sites: none (no resolved call, decorator or override edge reaches it)");
        return;
    }
    let pairs: Vec<Pair> = list.iter().take(MAX_ROWS).map(|&(c, _, d)| Pair { src: c, dst: v, dispatch: d }).collect();
    let sites = locate(cx.q, &pairs);
    let possible = list.iter().filter(|c| c.1 == Confidence::Possible && !c.2).count();
    doc.line(&format!("call sites: {} caller{} ({possible} only possible)", list.len(), if list.len() == 1 { "" } else { "s" }));
    for (k, &(c, conf, dispatch)) in list.iter().take(MAX_ROWS).enumerate() {
        let at = sites[k].first.map_or_else(String::new, |s| format!(" at {}:{}", doc.rel(cx.ix.file_path(s.file)), s.line));
        let state = liveness(cx, c);
        doc.line(&format!("  {} [{}{}]{at}: {state}", cx.ix.qid_string(c), conf.as_str(), if dispatch { ", dispatch" } else { "" }));
    }
    if list.len() > MAX_ROWS {
        doc.line(&format!("  ... {} more", list.len() - MAX_ROWS));
    }
}

/// Whether node `c` is live, in doubt or reported unused (with the finding's id).
fn liveness(cx: &Cx<'_>, c: NodeId) -> String {
    if let Some(p) = cx.finding_on(c, |g| (401..=404).contains(&g.rule.code())) {
        return format!("itself unused ({}, confidence {})", cx.ids[p], cx.items[p].confidence);
    }
    match cx.dead {
        Some(dc) if dc.level(c) >= 100 => "live".to_owned(),
        Some(dc) => format!("in doubt (liveness {}/100)", dc.level(c)),
        None => "unknown".to_owned(),
    }
}

/// First-party import statements of module `v`.
fn importers(cx: &Cx<'_>, doc: &mut Doc, v: NodeId) {
    let rows = super::arch::importers_of(cx, v);
    if rows.is_empty() {
        doc.line("imports of it: none in the analysed files");
        return;
    }
    doc.line(&format!("imports of it: {}", rows.len()));
    for r in rows.iter().take(MAX_ROWS) {
        let state = liveness(cx, r.module);
        doc.line(&format!("  {}:{} in {}: {state}", doc.rel(cx.ix.file_path(r.file)), r.line, cx.ix.qid_string(r.module)));
    }
}

/// Lines of first-party files where `name` occurs as a word (outside the definition line).
fn appearances(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, v: NodeId, name: &str) {
    let ix = cx.ix;
    let own = ix.file(v).zip(def_lines(cx, v).map(|d| d.1));
    let mut hits: Vec<(u32, u32)> = Vec::new();
    for fl in 0..ix.file_count() {
        if ix.file_origin(fl) != Origin::FirstParty {
            continue;
        }
        let Some(src) = sources.get(ix.file_path(fl)) else { continue };
        for n in word_lines(src, name) {
            if own != Some((fl, n)) {
                hits.push((fl, n));
            }
        }
    }
    if hits.is_empty() {
        doc.line(&format!("name `{name}`: appears nowhere else in the analysed Python files"));
        return;
    }
    doc.line(&format!("name `{name}` appears on {} other line{} (text search, any context):", hits.len(), if hits.len() == 1 { "" } else { "s" }));
    for &(fl, n) in hits.iter().take(MAX_APPEARANCES) {
        let path = ix.file_path(fl).to_path_buf();
        let text = sources.get(&path).map_or(String::new(), |s| s.line(n).trim().to_owned());
        doc.line(&format!("  {}:{n}: {}", doc.rel(&path), clip(&text)));
    }
    if hits.len() > MAX_APPEARANCES {
        doc.line(&format!("  ... {} more", hits.len() - MAX_APPEARANCES));
    }
}

/// At most 100 characters of `s`.
pub fn clip(s: &str) -> String {
    match s.char_indices().nth(100) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

/// Lines of the module at `path` with dynamic-access patterns (the cause of the penalty).
fn dynamic_access(doc: &mut Doc, sources: &mut Sources, path: &Path) {
    const PATTERNS: [&str; 6] = ["getattr(", "globals()", "vars(", "locals()", "__getattr__", "__dict__"];
    let Some(src) = sources.get(path) else { return };
    let mut lines: Vec<(u32, String)> = Vec::new();
    for n in 1..=src.len() {
        let l = src.line(n);
        if PATTERNS.iter().any(|p| l.contains(p)) {
            lines.push((n, l.trim().to_owned()));
        }
    }
    doc.line(&format!("dynamic access in {}: {} line{}", doc.rel(path), lines.len(), if lines.len() == 1 { "" } else { "s" }));
    for (n, l) in lines.iter().take(5) {
        doc.line(&format!("  {n}: {}", clip(l)));
    }
}

/// The dead-code mode of the project units and the entry points (roots) it starts from.
fn roots(cx: &Cx<'_>, doc: &mut Doc, v: NodeId) {
    for line in cx.modes {
        doc.line(&format!("mode: {line}"));
    }
    let Some(dc) = cx.dead else { return };
    let rs = dc.roots();
    let kinds = [
        (root_kind::MAIN, "__main__"),
        (root_kind::SCRIPT, "scripts"),
        (root_kind::TEST, "tests"),
        (root_kind::DECORATOR, "entrypoint decorators"),
        (root_kind::NAME, "entrypoint names"),
        (root_kind::BASE, "entrypoint bases"),
        (root_kind::PUBLIC, "public API"),
    ];
    let mut counts = [0usize; 7];
    let mut first: Vec<NodeId> = Vec::new();
    for r in rs.iter() {
        let k = rs.kind(r);
        for (i, (bit, _)) in kinds.iter().enumerate() {
            if k & bit != 0 {
                counts[i] += 1;
            }
        }
        if k & !root_kind::MODULE != 0 && k & (root_kind::PUBLIC | root_kind::TEST) == 0 && first.len() < 5 {
            first.push(r);
        }
    }
    let parts: Vec<String> =
        kinds.iter().zip(counts).filter(|(_, n)| *n > 0).map(|((_, name), n)| format!("{name} {n}")).collect();
    let mut line = format!("roots checked: {}", if parts.is_empty() { "none besides module-level code".to_owned() } else { parts.join(", ") });
    if !first.is_empty() {
        let names: Vec<String> = first.iter().map(|&r| cx.ix.qid_string(r)).collect();
        line.push_str(&format!(" (e.g. {})", names.join(", ")));
    }
    doc.line(&line);
    if rs.contains(v) {
        doc.line("note: it is itself a root, so only a filter or suppression explains the finding");
    }
}

/// What deleting `v` takes with it: contained definitions, the chain of code only it uses,
/// and the imports that name it.
fn effect(cx: &Cx<'_>, doc: &mut Doc, v: NodeId, module: bool) {
    let ix = cx.ix;
    doc.section("Effect of deleting it");
    let inside = ix.subtree(v).len().saturating_sub(1);
    if inside > 0 {
        doc.line(&format!("removes {inside} contained definition{} with it", if inside == 1 { "" } else { "s" }));
    }
    // Code reached from it whose only references come from unused code.
    let mut chain: Vec<NodeId> = Vec::new();
    let mut queue: Vec<NodeId> = ix.subtree(v).collect();
    let mut head = 0;
    while head < queue.len() && chain.len() < 64 {
        let u = queue[head];
        head += 1;
        for (w, _, dispatch) in cx.q.direct_callees(u) {
            if dispatch || ix.subtree(v).contains(&w) || chain.contains(&w) {
                continue;
            }
            let dead_ref = cx.finding_on(w, |g| (401..=404).contains(&g.rule.code()) && g.reasons & reason::ONLY_USED_BY_UNUSED_CODE != 0);
            if dead_ref.is_some() {
                chain.push(w);
                queue.push(w);
            }
        }
    }
    if chain.is_empty() {
        doc.line("no other code becomes unused (nothing it calls is used only by unused code)");
    } else {
        doc.line(&format!("also unused once it goes ({}):", chain.len()));
        for &w in chain.iter().take(MAX_ROWS) {
            let id = cx.finding_on(w, |g| (401..=404).contains(&g.rule.code())).map_or(String::new(), |p| format!(" {}", cx.ids[p]));
            doc.line(&format!("  {} at {}{id}", ix.qid_string(w), cx.def_at(doc, w)));
        }
    }
    // Import statements that name it (they break or become unused).
    let st = &ix.facts().imports;
    let mut rows: Vec<(u32, u32)> = Vec::new();
    for fl in 0..ix.file_count() {
        if ix.file_origin(fl) != Origin::FirstParty {
            continue;
        }
        for row in ix.file_facts(fl).imports.range() {
            let hit = if module { cx.imp.module_node(row) == v } else { cx.imp.target(row) == Some(v) };
            if hit {
                rows.push((fl, st.line[row]));
            }
        }
    }
    if rows.is_empty() {
        doc.line("no import statement names it");
    } else {
        doc.line(&format!("import statements to remove with it ({}):", rows.len()));
        for &(fl, line) in rows.iter().take(MAX_ROWS) {
            doc.line(&format!("  {}:{line}", doc.rel(ix.file_path(fl))));
        }
    }
}

/// Notebooks and compiled extensions under the project root: code pymapper does not read.
pub fn blind_spots(cx: &Cx<'_>, doc: &mut Doc) {
    let (notebooks, natives) = count_unscanned(cx.root);
    doc.section("Blind spots");
    doc.line(&format!(
        "not scanned in this repository: {notebooks} notebook{} (.ipynb), {natives} compiled extension{} (.so/.pyd); callbacks passed as values, getattr/string dispatch and calls from C are not edges",
        if notebooks == 1 { "" } else { "s" },
        if natives == 1 { "" } else { "s" }
    ));
}

/// `(notebooks, compiled extensions)` under `root`, skipping hidden directories, virtualenvs,
/// caches and build output. Bounded: stops counting after 100 000 entries.
fn count_unscanned(root: &Path) -> (usize, usize) {
    const SKIP: [&str; 6] = ["node_modules", "venv", "site-packages", "__pycache__", "build", "dist"];
    let (mut nb, mut so, mut seen) = (0, 0, 0usize);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 100_000 {
                return (nb, so);
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !name.starts_with('.') && !SKIP.contains(&name.as_ref()) {
                    stack.push(e.path());
                }
            } else if name.ends_with(".ipynb") {
                nb += 1;
            } else if name.ends_with(".so") || name.ends_with(".pyd") {
                so += 1;
            }
        }
    }
    (nb, so)
}

/// The unused-import explanation: the statement, its kind, `__all__` / re-export status and
/// other mentions of the name in the file.
pub fn unused_import(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, f: &Finding) {
    let ix = cx.ix;
    if f.loc.file.0 >= ix.file_count() {
        return;
    }
    let fl = f.loc.file.0;
    let path = ix.file_path(fl).to_path_buf();
    let name = match cx.findings.args(f).first() {
        Some(pymapper_core::rules::Arg::Name(n)) => pymapper_core::intern::Sym::from_raw(*n)
            .map_or_else(String::new, |s| String::from_utf8_lossy(ix.str(s)).into_owned()),
        _ => String::new(),
    };
    doc.section(&format!("Source {}:{}", doc.rel(&path), f.loc.line));
    if let Some(src) = sources.get(&path) {
        doc.quote(src, f.loc.line, f.loc.line, &[f.loc.line]);
    }
    doc.section("Evidence");
    let row = ix.file_facts(fl).imports.range().find(|&r| ix.facts().imports.line[r] == f.loc.line && ix.facts().imports.col[r] + 1 == f.loc.col);
    if let Some(row) = row {
        let mut kind = String::new();
        let flags = cx.imp.flags(row);
        if !super::arch::deferred_flags(flags) {
            kind.push_str("top-level");
        }
        for (bit, word) in super::arch::FLAG_WORDS {
            if flags & bit != 0 {
                if !kind.is_empty() {
                    kind.push_str(", ");
                }
                kind.push_str(word);
            }
        }
        doc.line(&format!("import kind: {kind}"));
    }
    for n in reason_names(f) {
        let text = reason_text(n);
        doc.line(&if text.is_empty() { n.to_owned() } else { format!("{n}: {text}") });
    }
    let init = path.file_name().is_some_and(|n| n == "__init__.py");
    doc.line(&format!("package __init__.py: {}", if init { "yes (imports here are often re-exports)" } else { "no" }));
    let exports = ix.file_facts(fl).exports.range();
    let in_all = exports.clone().any(|r| ix.str(ix.facts().exports[r]) == name.as_bytes());
    doc.line(&format!(
        "__all__: {}",
        if exports.is_empty() { "not defined".to_owned() } else if in_all { "lists it".to_owned() } else { "defined, does not list it".to_owned() }
    ));
    if let Some(src) = sources.get(&path) {
        let other: Vec<u32> = word_lines(src, &name).into_iter().filter(|&n| n != f.loc.line).collect();
        if other.is_empty() {
            doc.line(&format!("`{name}` appears nowhere else in the file (not in code, strings, annotations or cast())"));
        } else {
            doc.line(&format!("`{name}` also appears on line{} {} (text only: strings or comments)", if other.len() == 1 { "" } else { "s" }, join_nums(&other)));
        }
    }
    blind_spots(cx, doc);
    doc.section("Verify");
    doc.line(&format!("grep -nw {} {}", shell_word(&name), shell_word(&doc.rel(&path))));
}

/// `1, 5, 9` (at most 10, then `...`).
pub fn join_nums(v: &[u32]) -> String {
    let mut s: Vec<String> = v.iter().take(10).map(u32::to_string).collect();
    if v.len() > 10 {
        s.push("...".to_owned());
    }
    s.join(", ")
}
