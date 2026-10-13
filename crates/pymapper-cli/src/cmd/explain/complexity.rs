//! `explain` for complexity findings (PM101-107): every metric of the function against its
//! threshold, where the complexity comes from (a scan of the source text: branches and loops
//! with their nesting, the deepest nesting path, the largest nested blocks) and how risky a
//! refactor is (callers, impact).

use pymapper_core::index::NodeId;
use pymapper_core::query::{Scratch, WalkOptions};
use pymapper_core::resolve::Confidence;
use pymapper_core::rules::Arg;

use super::Cx;
use super::doc::{Doc, Source, Sources};
use crate::cmd::query::llm::metric_row;

/// Branch points listed at most.
const MAX_BRANCHES: usize = 8;

/// One control-flow line of the function.
struct Branch {
    line: u32,
    /// Enclosing control blocks (0 = directly in the function body).
    nesting: u32,
    /// Approximate cognitive cost: 1 + nesting for if/for/while/except/match, 1 for
    /// elif/else, plus 1 per boolean operator.
    cost: u32,
    keyword: &'static str,
    /// Last line of its block.
    end: u32,
    /// Lines of the enclosing control blocks, outermost first.
    path: Vec<u32>,
}

const KEYWORDS: [&str; 12] = ["if", "elif", "else", "for", "while", "try", "except", "finally", "with", "match", "case", "async"];

fn indent(s: &str) -> usize {
    s.len() - s.trim_start_matches([' ', '\t']).len()
}

/// The first word of a statement line if it is a control keyword (`async for` -> `for`).
fn keyword(stmt: &str) -> Option<&'static str> {
    let word = stmt.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or("");
    let k = KEYWORDS.iter().find(|&&k| k == word)?;
    if *k == "async" {
        return keyword(stmt[5..].trim_start()).filter(|k| *k == "for" || *k == "with");
    }
    Some(k)
}

/// Control-flow lines of the body `first..=last` (the def line excluded), skipping
/// continuation lines of bracketed expressions. A text scan: strings holding brackets can
/// mislead it, so the result is labelled approximate.
fn scan(src: &Source, def_line: u32, last: u32) -> Vec<Branch> {
    let mut out: Vec<Branch> = Vec::new();
    // Open control blocks: (indent, index into out).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0i32;
    let base = indent(src.line(def_line));
    for n in def_line + 1..=last.min(src.len()) {
        let raw = src.line(n);
        let stmt = raw.trim();
        let continuation = depth > 0;
        for c in stmt.chars() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '#' => break,
                _ => {}
            }
        }
        depth = depth.max(0);
        if continuation || stmt.is_empty() || stmt.starts_with('#') {
            continue;
        }
        let ind = indent(raw);
        if ind <= base {
            break;
        }
        while stack.last().is_some_and(|&(i, _)| i >= ind) {
            let (_, b) = stack.pop().unwrap_or((0, 0));
            out[b].end = n - 1;
        }
        let bools = (stmt.matches(" and ").count() + stmt.matches(" or ").count()) as u32;
        if let Some(k) = keyword(stmt) {
            let nesting = stack.len() as u32;
            let base_cost = match k {
                "if" | "for" | "while" | "except" | "match" => 1 + nesting,
                "elif" | "else" => 1,
                _ => 0,
            };
            let path = stack.iter().map(|&(_, b)| out[b].line).collect();
            out.push(Branch { line: n, nesting, cost: base_cost + bools, keyword: k, end: n, path });
            // `else`/`elif`/`except` continue a block at the same level: they open a block too.
            if stmt.ends_with(':') || k != "case" {
                stack.push((ind, out.len() - 1));
            }
        }
    }
    let end = last.min(src.len());
    for (_, b) in stack {
        out[b].end = end;
    }
    out
}

/// The complexity explanation of the metric findings `group` (one function).
pub fn explain(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, group: &[usize]) {
    let f = cx.items[group[0]];
    let Some(v) = Cx::node(f) else { return };
    metrics(cx, doc, v, group);
    let ix = cx.ix;
    let (Some(fl), Some(r)) = (ix.file(v), ix.def_row(v)) else { return };
    let d = &ix.facts().defs;
    let (first, def_line, last) = (d.start_line[r], d.name_line[r], d.end_line[r]);
    let path = ix.file_path(fl).to_path_buf();
    let rel = doc.rel(&path);
    let Some(src) = sources.get(&path) else {
        doc.line("(cannot read the source)");
        return;
    };
    let branches = scan(src, def_line, last);
    doc.section(&format!("Where the complexity comes from ({rel}:{first}-{last}, approximate, from the source text)"));
    let nested_defs = (def_line + 1..=last.min(src.len()))
        .filter(|&n| {
            let s = src.line(n).trim_start();
            s.starts_with("def ") || s.starts_with("async def ") || s.starts_with("class ")
        })
        .count();
    if nested_defs > 0 {
        doc.line(&format!(
            "contains {nested_defs} nested function{}/class{}: McCabe and cognitive include them (ruff C901 semantics)",
            if nested_defs == 1 { "" } else { "s" },
            if nested_defs == 1 { "" } else { "es" }
        ));
    }
    let mut ranked: Vec<&Branch> = branches.iter().filter(|b| b.cost > 0).collect();
    ranked.sort_by(|a, b| b.cost.cmp(&a.cost).then(a.line.cmp(&b.line)));
    if ranked.is_empty() {
        doc.line("no branches or loops found in the text");
    }
    for b in ranked.iter().take(MAX_BRANCHES) {
        doc.line(&format!("line {}: {} at nesting {} (cost ~{}): {}", b.line, b.keyword, b.nesting, b.cost, super::dead::clip(src.line(b.line).trim())));
    }
    if ranked.len() > MAX_BRANCHES {
        let rest: u32 = ranked[MAX_BRANCHES..].iter().map(|b| b.cost).sum();
        doc.line(&format!("... {} more branch points (cost ~{rest})", ranked.len() - MAX_BRANCHES));
    }
    if let Some(deep) = branches.iter().max_by(|a, b| a.nesting.cmp(&b.nesting).then(b.line.cmp(&a.line))) {
        let mut chain: Vec<String> = deep.path.iter().map(|l| l.to_string()).collect();
        chain.push(deep.line.to_string());
        doc.line(&format!("deepest nesting: {} levels, lines {}", deep.nesting + 1, chain.join(" > ")));
    }
    // Split points: the largest blocks that do not span the whole body.
    let body = last.saturating_sub(def_line);
    let mut blocks: Vec<&Branch> =
        branches.iter().filter(|b| b.end > b.line + 2 && (b.end - b.line) < body.saturating_sub(1) && b.keyword != "else").collect();
    blocks.sort_by(|a, b| (b.end - b.line).cmp(&(a.end - a.line)).then(a.line.cmp(&b.line)));
    if !blocks.is_empty() {
        doc.section("Split points (largest nested blocks)");
        for b in blocks.iter().take(3) {
            doc.line(&format!("lines {}-{} ({} lines): {} block at nesting {}", b.line, b.end, b.end - b.line + 1, b.keyword, b.nesting));
        }
    }
    doc.section("Refactor risk");
    let (mut real, mut possible) = (0, 0);
    for (_, c, dispatch) in cx.q.direct_callers(v) {
        if dispatch {
            continue;
        }
        if c == Confidence::Possible { possible += 1 } else { real += 1 }
    }
    let imp = cx.q.impact(v, &WalkOptions::transitive(), &mut Scratch::default());
    doc.line(&format!(
        "{real} direct caller{} ({possible} possible); impact: {} affected symbol{} in {} file{}, {} in tests",
        if real == 1 { "" } else { "s" },
        imp.walk.hops.len(),
        if imp.walk.hops.len() == 1 { "" } else { "s" },
        imp.files,
        if imp.files == 1 { "" } else { "s" },
        imp.tests
    ));
}

/// Every metric of `v` against its threshold; the reported ones from the findings' values.
fn metrics(cx: &Cx<'_>, doc: &mut Doc, v: NodeId, group: &[usize]) {
    doc.section("Metrics (value vs threshold)");
    let t = &cx.config.thresholds;
    let reported = |code: u16| -> Option<(String, String)> {
        group.iter().map(|&p| cx.items[p]).find(|f| f.rule.code() == code).map(|f| {
            let args = cx.findings.args(f);
            let num = |a: Option<&Arg>| match a {
                Some(Arg::U32(x)) => x.to_string(),
                Some(Arg::F32(x)) => format!("{x:.1}"),
                _ => "?".to_owned(),
            };
            (num(args.get(1)), num(args.get(2)))
        })
    };
    let ix = cx.ix;
    let mx = ix.metrics();
    let row = metric_row(ix, v);
    let measured = |col: &dyn Fn(usize) -> String| row.map_or_else(|| "?".to_owned(), col);
    let lines: [(u16, &str, String, String, bool); 6] = [
        (101, "McCabe", measured(&|m| mx.cyclomatic[m].to_string()), t.max_mccabe.to_string(), false),
        (102, "cognitive", measured(&|m| mx.cognitive[m].to_string()), t.max_cognitive.to_string(), false),
        (103, "nesting", measured(&|m| mx.max_nesting[m].to_string()), t.max_nesting.to_string(), false),
        (104, "lines (excluding docstrings)", measured(&|m| mx.code_lines(m).to_string()), t.max_function_loc.to_string(), false),
        (105, "parameters (excluding self/cls)", String::from("?"), t.max_params.to_string(), false),
        (106, "maintainability index", measured(&|m| format!("{:.1}", mx.mi(m))), format!("{:.1}", t.min_maintainability), true),
    ];
    for (code, name, value, threshold, min) in lines {
        let (value, threshold, over) = match reported(code) {
            Some((v, th)) => (v, th, true),
            None => (value, threshold, false),
        };
        if code == 105 && !over {
            // Parameters are only measured exactly for the finding (self/cls rules).
            let p = ix.def_row(v).map_or(0, |r| ix.facts().defs.params_len[r]);
            doc.line(&format!("{name}: {p} declared (limit {threshold})"));
            continue;
        }
        let rel = if over { if min { "<" } else { ">" } } else if min { ">=" } else { "<=" };
        let tag = if over { format!(" PM{code} over") } else { String::new() };
        doc.line(&format!("{name}: {value} {rel} {threshold}{tag}"));
    }
}
