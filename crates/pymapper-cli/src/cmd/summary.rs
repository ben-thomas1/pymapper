//! `pymapper summary` subcommand: the program at a glance for an agent or a human starting on
//! a codebase.
//!
//! ```text
//!   load (as check) ──> calls ──> Query::summary_where (one O(V + E) pass: counts per
//!   origin, resolution rates, cycle SCCs, bounded top-k rankings over files under PATHS)
//!   ──> concise | json ──> exit 0
//! ```
//!
//! Counts always cover the whole loaded program; positional paths restrict the rankings.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::query::{OriginCounts, Ranked, Summary, SummaryOptions};
use pymapper_core::timing;

use super::query::{QueryFlags, Session, Writer, finish};

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
    timing::time("output", || if w.json { write_json(&mut w, &sum) } else { write_concise(&mut w, &sum) });
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
