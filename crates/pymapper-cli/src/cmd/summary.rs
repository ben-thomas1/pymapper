//! `pymapper summary` subcommand: the program at a glance for an agent or a human starting on
//! a codebase.
//!
//! ```text
//!   load (as check) ──> calls ──> Query::summary_where (one O(V + E) pass: counts per
//!   origin, resolution rates, cycle SCCs, bounded top-k rankings over files under PATHS)
//!   ──> text | llm | json ──> exit 0
//! ```
//!
//! Counts always cover the whole loaded program; positional paths restrict the rankings.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::analysis::testfiles;
use pymapper_core::index::NodeId;
use pymapper_core::query::{OriginCounts, Query, Ranked, Summary, SummaryOptions};
use pymapper_core::resolve::Confidence;
use pymapper_core::roots::Origin;
use pymapper_core::timing;

use super::query::{QueryFlags, Session, Writer, finish, llm};

#[derive(clap::Args)]
pub struct Args {
    /// Rank only code in these files and directories (default: everything). Counts cover the
    /// whole project, which is loaded either way.
    paths: Vec<PathBuf>,
    /// Entries per ranking.
    #[arg(long, value_name = "N", default_value_t = 10)]
    top: usize,
    /// Rank test code too.
    #[arg(long)]
    include_tests: bool,
    /// Rank third-party and stdlib code too (when loaded with --deps / --stdlib).
    #[arg(long)]
    all_origins: bool,
    #[command(flatten)]
    flags: QueryFlags,
}

const ORIGINS: [&str; 3] = ["first-party", "third-party", "stdlib"];

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let s = Session::open("summary", args.flags, args.paths)?;
    let q = s.query();
    let opts = SummaryOptions { top: args.top, first_party_only: !args.all_origins, include_tests: args.include_tests };
    let sum = timing::time("query", || q.summary_where(&opts, |f| s.file_in_scope(f)));
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    timing::time("output", || {
        if w.json {
            write_json(&mut w, &sum)
        } else if w.llm {
            let called = most_called(&s, &q, &opts);
            write_llm(&s, &mut w, &sum, &called)
        } else {
            write_concise(&mut w, &sum)
        }
    });
    finish(w)?;
    s.report_stats(&q, &[]);
    Ok(ExitCode::SUCCESS)
}

/// `n` followed by `one` or `many`.
fn count(w: &mut Writer<'_>, n: u64, one: &str, many: &str) {
    w.num(n);
    w.str(" ");
    w.str(if n == 1 { one } else { many });
}

/// `part` of `whole` as a percentage with one decimal (`0.0%` for an empty whole).
fn percent(w: &mut Writer<'_>, part: u64, whole: u64) {
    let tenths = (part * 1000 + whole / 2).checked_div(whole).unwrap_or(0);
    w.num(tenths / 10);
    w.str(".");
    w.num(tenths % 10);
    w.str("%");
}

fn write_concise(w: &mut Writer<'_>, s: &Summary) {
    for (i, c) in s.origins.iter().enumerate() {
        if c.files == 0 && i > 0 {
            continue;
        }
        w.str(ORIGINS[i]);
        w.str(": ");
        count(w, u64::from(c.files), "file", "files");
        w.str(", ");
        count(w, c.lines, "line", "lines");
        w.str(", ");
        count(w, u64::from(c.packages), "package", "packages");
        w.str(", ");
        count(w, u64::from(c.modules), "module", "modules");
        w.str(", ");
        count(w, u64::from(c.classes), "class", "classes");
        w.str(", ");
        count(w, u64::from(c.functions), "function", "functions");
        w.end_line();
    }
    w.str("imports: ");
    count(w, s.import_rows, "row", "rows");
    w.str(", ");
    w.num(s.unresolved_imports);
    w.str(" unresolved (");
    percent(w, s.unresolved_imports, s.import_rows);
    w.str("), ");
    count(w, s.import_edges, "module edge", "module edges");
    w.end_line();
    w.str("calls: ");
    count(w, s.call_sites, "site", "sites");
    w.str(": ");
    for (i, name) in ["certain", "likely", "possible", "unresolved"].iter().enumerate() {
        if i > 0 {
            w.str(", ");
        }
        percent(w, s.calls_by_conf[i], s.call_sites);
        w.str(" ");
        w.str(name);
    }
    w.str("; ");
    count(w, s.call_edges, "caller-callee pair", "caller-callee pairs");
    w.end_line();
    w.str("import cycles: ");
    w.num(u64::from(s.import_cycles));
    if s.import_cycles > 0 {
        w.str(" (largest: ");
        count(w, u64::from(s.largest_import_cycle), "module)", "modules)");
    }
    w.end_line();
    let rankings: [(&str, &[Ranked], &str, &str); 4] = [
        ("cognitive", &s.cognitive, "", ""),
        ("cyclomatic", &s.cyclomatic, "", ""),
        ("most called", &s.most_called, "caller", "callers"),
        ("most imported", &s.most_imported, "importer", "importers"),
    ];
    for (name, rows, one, many) in rankings {
        for r in rows.iter().filter(|r| r.value > 0) {
            w.node_loc(r.node);
            w.str(name);
            w.str(" ");
            w.qid(r.node);
            w.str(" ");
            if one.is_empty() {
                w.num(u64::from(r.value));
            } else {
                w.str("(");
                count(w, u64::from(r.value), one, many);
                w.str(")");
            }
            w.end_line();
        }
    }
}

fn origin_json(w: &mut Writer<'_>, c: &OriginCounts) {
    w.str("{");
    w.key("files", true);
    w.num(u64::from(c.files));
    w.key_num("lines", c.lines);
    w.key_num("packages", u64::from(c.packages));
    w.key_num("modules", u64::from(c.modules));
    w.key_num("classes", u64::from(c.classes));
    w.key_num("functions", u64::from(c.functions));
    w.str("}");
}

fn ranking_json(w: &mut Writer<'_>, key: &str, rows: &[Ranked]) {
    w.open_array(key);
    let rows: Vec<&Ranked> = rows.iter().filter(|r| r.value > 0).collect();
    for (i, r) in rows.iter().enumerate() {
        w.item(i);
        w.str("{");
        w.symbol_fields(r.node, true);
        w.key_num("value", u64::from(r.value));
        w.str("}");
    }
    w.close_array(rows.len());
}

fn write_json(w: &mut Writer<'_>, s: &Summary) {
    w.json_header("summary");
    w.key("origins", false);
    w.str("{");
    for (i, c) in s.origins.iter().enumerate() {
        w.key(ORIGINS[i], i == 0);
        origin_json(w, c);
    }
    w.str("}");
    w.key("imports", false);
    w.str("{");
    w.key("rows", true);
    w.num(s.import_rows);
    w.key_num("unresolved", s.unresolved_imports);
    w.key_num("module_edges", s.import_edges);
    w.str("}");
    w.key("calls", false);
    w.str("{");
    w.key("sites", true);
    w.num(s.call_sites);
    for (i, name) in ["certain", "likely", "possible", "unresolved"].iter().enumerate() {
        w.key_num(name, s.calls_by_conf[i]);
    }
    w.key_num("pairs", s.call_edges);
    w.str("}");
    w.key("import_cycles", false);
    w.str("{");
    w.key("count", true);
    w.num(u64::from(s.import_cycles));
    w.key_num("largest", u64::from(s.largest_import_cycle));
    w.str("}");
    ranking_json(w, "cognitive", &s.cognitive);
    ranking_json(w, "cyclomatic", &s.cyclomatic);
    ranking_json(w, "most_called", &s.most_called);
    ranking_json(w, "most_imported", &s.most_imported);
    w.json_end();
}

/// The most called classes and functions counting only `certain` and `likely` callers:
/// a `possible` edge is a guess from a unique method name and inflates the ranking of the one
/// first-party override (ISSUES F9). Same candidates as [`Query::summary_where`]'s ranking.
fn most_called(s: &Session, q: &Query<'_>, opts: &SummaryOptions) -> Vec<(NodeId, u32, u32)> {
    let ix = q.index();
    let mut out: Vec<(NodeId, u32, u32)> = Vec::new();
    for v in 0..ix.node_count() {
        let Some(f) = ix.file(v) else { continue };
        if !ix.kind(v).is_def()
            || (opts.first_party_only && ix.file_origin(f) != Origin::FirstParty)
            || (!opts.include_tests && testfiles::is_test_file(ix, f))
            || !s.file_in_scope(f)
        {
            continue;
        }
        let (mut real, mut possible) = (0u32, 0u32);
        for (_, conf, dispatch) in q.direct_callers(v) {
            if dispatch {
                continue;
            }
            if conf == Confidence::Possible { possible += 1 } else { real += 1 }
        }
        if real > 0 {
            out.push((v, real, possible));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(opts.top);
    out
}

/// The llm rendering: sizes and resolution rates (how far the call graph can be trusted),
/// then the rankings, complexity merged per function.
fn write_llm(s: &Session, w: &mut Writer<'_>, sum: &Summary, called: &[(NodeId, u32, u32)]) {
    llm::header(w, "summary", "");
    llm::root(w, s);
    w.end_line();
    for (i, c) in sum.origins.iter().enumerate() {
        if c.files == 0 && i > 0 {
            continue;
        }
        w.str(ORIGINS[i]);
        w.str(": ");
        count(w, u64::from(c.files), "file", "files");
        w.str(", ");
        count(w, c.lines, "line", "lines");
        w.str(", ");
        count(w, u64::from(c.modules), "module", "modules");
        w.str(", ");
        count(w, u64::from(c.classes), "class", "classes");
        w.str(", ");
        count(w, u64::from(c.functions), "function", "functions");
        w.end_line();
    }
    w.str("imports: ");
    count(w, sum.import_rows, "row", "rows");
    w.str(", ");
    w.num(sum.unresolved_imports);
    w.str(" unresolved · import cycles: ");
    w.num(u64::from(sum.import_cycles));
    if sum.import_cycles > 0 {
        w.str(" (largest ");
        count(w, u64::from(sum.largest_import_cycle), "module)", "modules)");
    }
    w.end_line();
    w.str("calls: ");
    count(w, sum.call_sites, "site", "sites");
    w.str(": ");
    for (i, name) in ["certain", "likely", "possible", "unresolved"].iter().enumerate() {
        if i > 0 {
            w.str(", ");
        }
        percent(w, sum.calls_by_conf[i], sum.call_sites);
        w.str(" ");
        w.str(name);
    }
    w.end_line();
    w.str("Caller counts are lower bounds: unresolved calls, callbacks passed as values, getattr dispatch and calls from C extensions are not edges.");
    w.end_line();
    // Complexity: the union of both rankings, worst cognitive first, with both values.
    let ix = w.ix;
    let mut complex: Vec<(NodeId, u32, u32)> = Vec::new();
    for r in sum.cognitive.iter().chain(&sum.cyclomatic).filter(|r| r.value > 0) {
        if complex.iter().any(|c| c.0 == r.node) {
            continue;
        }
        let Some(m) = llm::metric_row(ix, r.node) else { continue };
        let mx = ix.metrics();
        complex.push((r.node, u32::from(mx.cognitive[m]), u32::from(mx.cyclomatic[m])));
    }
    complex.sort_by(|a, b| (b.1, b.2).cmp(&(a.1, a.2)).then(a.0.cmp(&b.0)));
    if !complex.is_empty() {
        w.end_line();
        w.str("## Most complex functions (cognitive, cyclomatic)");
        w.end_line();
        for &(v, cog, cc) in &complex {
            w.def_at(v);
            w.str(" ");
            w.short(v);
            w.str(": cognitive ");
            w.num(u64::from(cog));
            w.str(", cyclomatic ");
            w.num(u64::from(cc));
            w.end_line();
        }
    }
    if !called.is_empty() {
        w.end_line();
        w.str("## Most called (distinct certain/likely callers; possible callers not counted)");
        w.end_line();
        for &(v, real, possible) in called {
            w.def_at(v);
            w.str(" ");
            w.short(v);
            w.str(": ");
            count(w, u64::from(real), "caller", "callers");
            if possible > 0 {
                w.str(" (+");
                w.num(u64::from(possible));
                w.str(" possible)");
            }
            w.end_line();
        }
    }
    let imported: Vec<&Ranked> = sum.most_imported.iter().filter(|r| r.value > 0).collect();
    if !imported.is_empty() {
        w.end_line();
        w.str("## Most imported (importing modules)");
        w.end_line();
        for r in imported {
            w.qid(r.node);
            w.str(": ");
            w.num(u64::from(r.value));
            w.end_line();
        }
    }
    llm::next(w, "`pymapper check --llm` for findings · `pymapper callers <symbol>` / `pymapper impact <symbol>` before changing a hub");
}
