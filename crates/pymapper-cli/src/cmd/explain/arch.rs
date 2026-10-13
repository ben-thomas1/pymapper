//! `explain` for architecture findings (import cycles PM301, contracts PM302-304) and
//! duplication (PM501), plus the import helpers the other families share.

use pymapper_core::config::ContractKind;
use pymapper_core::index::{FileId, NodeId};
use pymapper_core::resolve::res_flags;
use pymapper_core::roots::Origin;
use pymapper_core::rules::{Arg, Finding};

use super::Cx;
use super::dead::{blind_spots, clip};
use super::doc::{Doc, Sources, word_lines};
use crate::cmd::cycles::{CycleRow, cycle_rows};

/// Words for the deferring import flags, in display order.
pub const FLAG_WORDS: [(u8, &str); 5] = [
    (res_flags::LAZY, "inside a function"),
    (res_flags::TYPE_ONLY, "TYPE_CHECKING"),
    (res_flags::TRY, "try"),
    (res_flags::CONDITIONAL, "conditional"),
    (res_flags::STAR, "star"),
];

/// Whether import flags defer or guard an import (it does not run on import).
pub fn deferred_flags(flags: u8) -> bool {
    flags & (res_flags::TYPE_ONLY | res_flags::CONDITIONAL | res_flags::TRY | res_flags::LAZY) != 0
}

/// `top-level` or the deferring flags of an import (`inside a function, try`).
pub fn kind_words(flags: u8) -> String {
    let mut out = String::new();
    if !deferred_flags(flags) {
        out.push_str("top-level");
    }
    for (bit, word) in FLAG_WORDS {
        if flags & bit != 0 {
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(word);
        }
    }
    out
}

/// One first-party module importing another: its first import statement of it.
pub struct ImportRow {
    pub module: NodeId,
    pub file: FileId,
    pub line: u32,
}

/// First-party modules importing `module` (any import kind), each at its first statement.
/// O(first-party import rows).
pub fn importers_of(cx: &Cx<'_>, module: NodeId) -> Vec<ImportRow> {
    let ix = cx.ix;
    let st = &ix.facts().imports;
    let mut out = Vec::new();
    for f in 0..ix.file_count() {
        if ix.file_origin(f) != Origin::FirstParty || ix.file_node(f) == module {
            continue;
        }
        if let Some(row) = ix.file_facts(f).imports.range().find(|&r| cx.imp.module_node(r) == module) {
            out.push(ImportRow { module: ix.file_node(f), file: f, line: st.line[row] });
        }
    }
    out
}

/// The name an import row binds (alias, imported name, or the first segment of `import a.b`).
fn bound_name(cx: &Cx<'_>, row: usize) -> String {
    let st = &cx.ix.facts().imports;
    let sym = st.alias[row].or(st.name[row]);
    match sym {
        Some(s) => String::from_utf8_lossy(cx.ix.str(s)).into_owned(),
        None => {
            // Plain `import a.b`: the module chain's first name.
            let text = String::from_utf8_lossy(cx.ix.str(st.module[row])).into_owned();
            text.split('.').next().unwrap_or("").to_owned()
        }
    }
}

/// The import cycle explanation: members, every import between them with its kind and
/// source, whether it runs on import, and the cheapest edge to break.
pub fn cycle(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, f: &Finding) {
    let ix = cx.ix;
    let members: Vec<NodeId> = cx.findings.extra(f).iter().filter_map(|a| if let Arg::Symbol(s) = a { Some(s.0) } else { None }).collect();
    doc.section("Members");
    let names: Vec<String> = members.iter().map(|&m| ix.qid_string(m)).collect();
    doc.line(&format!("{} modules: {}", members.len(), names.join(", ")));
    let rows = cycle_rows(cx.q, &members);
    doc.section(&format!("Import statements between members ({})", rows.len()));
    for r in &rows {
        let path = ix.file_path(r.file).to_path_buf();
        let text = sources.get(&path).map_or(String::new(), |s| s.line(r.line).trim().to_owned());
        doc.line(&format!(
            "{}:{} {} -> {} ({}): {}",
            doc.rel(&path),
            r.line,
            ix.qid_string(r.src),
            ix.qid_string(r.dst),
            kind_words(r.flags),
            clip(&text)
        ));
    }
    doc.section("Runtime");
    let eager: Vec<&CycleRow> = rows.iter().filter(|r| !deferred_flags(r.flags)).collect();
    if eager_cycle(&members, &eager) {
        doc.line("a real runtime cycle: the top-level imports alone close it, so importing any member first runs into a partly initialised module");
    } else {
        doc.line("not a runtime cycle: every way around it goes through a deferred, guarded or TYPE_CHECKING import");
    }
    // Cheapest edge: a top-level import whose bound name is used only in indented code
    // (inside functions or classes), so it can move into them.
    doc.section("Cheapest edge to break (from the source text)");
    let mut found = false;
    for r in &eager {
        let path = ix.file_path(r.file).to_path_buf();
        let name = bound_name(cx, r.row);
        let Some(src) = sources.get(&path) else { continue };
        let uses: Vec<u32> = word_lines(src, &name).into_iter().filter(|&n| n != r.line).collect();
        let indented = uses.iter().all(|&n| src.line(n).starts_with([' ', '\t']));
        if !uses.is_empty() && indented {
            found = true;
            doc.line(&format!(
                "{}:{}: `{name}` is used only inside functions or classes (lines {}): move this import into them",
                doc.rel(&path),
                r.line,
                super::dead::join_nums(&uses)
            ));
        }
    }
    if !found {
        doc.line("no top-level import is used only inside functions; move the shared names into a module both can import, or invert one dependency");
    }
    blind_spots(cx, doc);
}

/// Whether the `edges` (top-level imports between `members`) contain a cycle. Iterative DFS
/// with three colours; O(members + edges).
fn eager_cycle(members: &[NodeId], edges: &[&CycleRow]) -> bool {
    let idx = |v: NodeId| members.iter().position(|&m| m == v);
    let n = members.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if let (Some(a), Some(b)) = (idx(e.src), idx(e.dst)) {
            adj[a].push(b);
        }
    }
    let mut colour = vec![0u8; n];
    for s in 0..n {
        if colour[s] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(s, 0)];
        colour[s] = 1;
        while let Some(&mut (u, ref mut k)) = stack.last_mut() {
            if *k < adj[u].len() {
                let w = adj[u][*k];
                *k += 1;
                match colour[w] {
                    0 => {
                        colour[w] = 1;
                        stack.push((w, 0));
                    }
                    1 => return true,
                    _ => {}
                }
            } else {
                colour[u] = 2;
                stack.pop();
            }
        }
    }
    false
}

/// Module lists joined for display.
fn list(v: &[String]) -> String {
    v.join(", ")
}

/// The contract explanation: the contract as configured, the offending import and the other
/// modules that import the same target.
pub fn contract(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, f: &Finding) {
    let ix = cx.ix;
    let args = cx.findings.args(f);
    doc.section("Contract");
    if let Some(c) = args.iter().find_map(|a| if let Arg::Contract(i) = a { cx.config.contracts.get(*i as usize) } else { None }) {
        let what = match &c.kind {
            ContractKind::Layers { layers, containers, independent_siblings } => {
                let ls: Vec<String> = layers.iter().map(|l| l.join(" | ")).collect();
                format!(
                    "layers (highest first): {}{}{}",
                    ls.join(" > "),
                    if containers.is_empty() { String::new() } else { format!(" in {}", list(containers)) },
                    if *independent_siblings { "; siblings independent" } else { "" }
                )
            }
            ContractKind::Forbidden { source, forbidden } => format!("forbidden: {} may not import {}", list(source), list(forbidden)),
            ContractKind::Independence { modules } => format!("independence: none of {} may import another", list(modules)),
        };
        doc.line(&format!("\"{}\": {what}", c.name));
        doc.line(&format!(
            "TYPE_CHECKING imports {}; {}; {} ignored import pattern{}",
            if c.allow_type_checking { "exempt" } else { "checked" },
            if c.transitive { "indirect imports checked" } else { "direct imports only" },
            c.ignore_imports.len(),
            if c.ignore_imports.len() == 1 { "" } else { "s" }
        ));
    }
    if f.loc.file.0 < ix.file_count() && f.loc.line > 0 {
        let path = ix.file_path(f.loc.file.0).to_path_buf();
        doc.section(&format!("Offending import {}:{}", doc.rel(&path), f.loc.line));
        if let Some(src) = sources.get(&path) {
            doc.quote(src, f.loc.line, f.loc.line, &[f.loc.line]);
        }
        if let Some(row) = ix.file_facts(f.loc.file.0).imports.range().find(|&r| ix.facts().imports.line[r] == f.loc.line) {
            doc.line(&format!("kind: {}", kind_words(cx.imp.flags(row))));
        }
    }
    // The imported module: the second argument (a symbol, or a name for PM303).
    let target = match args.get(1) {
        Some(Arg::Symbol(s)) => Some(s.0),
        _ if f.loc.file.0 < ix.file_count() => ix
            .file_facts(f.loc.file.0)
            .imports
            .range()
            .find(|&r| ix.facts().imports.line[r] == f.loc.line)
            .map(|row| cx.imp.module_node(row)),
        _ => None,
    };
    if let Some(t) = target.filter(|&t| t < ix.node_count()) {
        let importer = Cx::node(f);
        let others: Vec<ImportRow> = importers_of(cx, t).into_iter().filter(|r| Some(r.module) != importer).collect();
        doc.section(&format!("Other modules importing {} ({})", ix.qid_string(t), others.len()));
        for r in others.iter().take(10) {
            doc.line(&format!("{} at {}:{}", ix.qid_string(r.module), doc.rel(ix.file_path(r.file)), r.line));
        }
        if others.len() > 10 {
            doc.line(&format!("... {} more", others.len() - 10));
        }
    }
}

/// The duplication explanation: both spans with their source and how they differ.
pub fn duplication(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, f: &Finding) {
    let ix = cx.ix;
    let extra = cx.findings.extra(f);
    let [Arg::File(of), Arg::U32(oline), Arg::U32(oend), Arg::Symbol(osym)] = *extra else { return };
    if f.loc.file.0 >= ix.file_count() || of.0 >= ix.file_count() {
        return;
    }
    let end = if f.end_line >= f.loc.line { f.end_line } else { f.loc.line };
    let a_path = ix.file_path(f.loc.file.0).to_path_buf();
    let b_path = ix.file_path(of.0).to_path_buf();
    let mut spans: Vec<(String, Vec<String>)> = Vec::new();
    for (path, first, last, sym) in [(&a_path, f.loc.line, end, f.symbol), (&b_path, oline, oend, osym)] {
        let title = format!(
            "{}:{first}-{last}{}",
            doc.rel(path),
            if sym.0 < ix.node_count() { format!(" ({})", ix.qid_string(sym.0)) } else { String::new() }
        );
        doc.section(&format!("Copy {title}"));
        let Some(src) = sources.get(path) else {
            doc.line("(cannot read the file)");
            continue;
        };
        doc.quote(src, first, last, &[]);
        spans.push((title, (first..=last.min(src.len())).map(|n| src.line(n).trim().to_owned()).collect()));
    }
    if spans.len() != 2 {
        return;
    }
    doc.section("Comparison");
    let (a, b) = (&spans[0].1, &spans[1].1);
    let diffs: Vec<usize> = (0..a.len().max(b.len())).filter(|&i| a.get(i) != b.get(i)).collect();
    if diffs.is_empty() {
        doc.line(&format!("identical ({} lines, ignoring indentation)", a.len()));
    } else {
        doc.line(&format!("{} of {} lines differ (the match is token-based; these lines differ in names, literals or docstrings):", diffs.len(), a.len().max(b.len())));
        for &i in diffs.iter().take(8) {
            let x = a.get(i).map_or("", String::as_str);
            let y = b.get(i).map_or("", String::as_str);
            doc.line(&format!("  {}: {}  |  {}", i + 1, clip(x), clip(y)));
        }
        if diffs.len() > 8 {
            doc.line(&format!("  ... {} more", diffs.len() - 8));
        }
    }
}
