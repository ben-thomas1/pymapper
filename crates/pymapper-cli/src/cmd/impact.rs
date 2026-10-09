//! `pymapper impact` subcommand: everything that transitively calls a symbol (what a change
//! to it can break), with the modules importing it.
//!
//! ```text
//!   load (as check) ──> calls ──> Query ──> resolve SYMBOL ──> Query::impact (reverse BFS,
//!   per-module counts, tests, files, transitive importers) ──> PATHS filter ──> concise | json
//! ```
//!
//! Rows carry the least sure edge on the path back to the symbol: `[possible]` means at least
//! one hop of the chain is a guess. Counts in the summary cover the whole program.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::index::NodeId;
use pymapper_core::query::{Impact, Query, Scratch};
use pymapper_core::timing;

use super::query::hops::{self, RowCounts, Side};
use super::query::{QueryFlags, Session, WalkFlags, Writer, finish, import_site};

#[derive(clap::Args)]
pub struct Args {
    /// The symbol: a qualified id (pkg.mod:Class.method), a dotted name (pkg.mod.Class.method)
    /// or its last segments (Class.method, method). A class, module or package stands for
    /// everything in it (see --scope).
    symbol: String,
    /// Show only rows in these files and directories (default: everything). The whole project
    /// is loaded either way; config and project-root discovery start at the first.
    paths: Vec<PathBuf>,
    /// Hops from the symbol (default: every hop).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    depth: Option<u32>,
    #[command(flatten)]
    walk: WalkFlags,
    #[command(flatten)]
    flags: QueryFlags,
}

/// Most affected modules named in the concise summary line.
const TOP_MODULES: usize = 3;

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let s = Session::open("impact", args.flags, args.paths)?;
    let q = s.query();
    let target = match s.resolve(&q, "impact", &args.symbol)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let depth = args.depth.unwrap_or(u32::MAX);
    let opts = args.walk.walk(depth);
    let mut scratch = Scratch::default();
    let imp = timing::time("query", || q.impact(target, &opts, &mut scratch));
    let limit = args.walk.row_limit();
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    let counts = timing::time("output", || {
        if w.json {
            w.json_header("impact");
            w.key("target", false);
            w.symbol_object(target);
            w.key("depth", false);
            if depth == u32::MAX { w.str("null") } else { w.num(u64::from(depth)) }
            w.open_array("results");
        }
        let rows = hops::write_rows(&s, &q, &mut w, &imp.walk, Side::Callers, limit, true);
        if w.json {
            w.close_array(rows.shown);
        }
        // One cap for both sections: importers get what the call rows left.
        let importers = write_importers(&s, &q, &mut w, target, &imp, limit - rows.shown);
        if w.json {
            w.open_array("modules");
            for (i, &(m, n)) in imp.modules.iter().enumerate() {
                w.item(i);
                w.str("{");
                w.symbol_fields(m, true);
                w.key_num("affected", u64::from(n));
                w.str("}");
            }
            w.close_array(imp.modules.len());
            w.key("summary", false);
            w.str("{");
            w.key("results", true);
            w.num(rows.matched as u64);
            w.key_num("symbols", u64::from(imp.symbols));
            w.key_num("module_scopes", u64::from(imp.module_scopes));
            w.key_num("tests", u64::from(imp.tests));
            w.key_num("files", u64::from(imp.files));
            w.key_num("importers", imp.importers.len() as u64);
            hops::json_counts(&mut w, &rows);
            w.key_num("importers_shown", importers.shown as u64);
            w.str("}");
            w.json_end();
        } else {
            summary_line(&mut w, target, &imp, &rows, &importers);
        }
        rows
    });
    finish(w)?;
    s.report_stats(&q, &[("reached", imp.walk.hops.len() as u64), ("importers", imp.importers.len() as u64), ("shown", counts.shown as u64)]);
    Ok(ExitCode::SUCCESS)
}

/// Writes the transitive importers under the positional paths, each at its first import
/// statement of the module that brought it in (concise
/// `path:line:col: qid [importer] imports qid +N rows`; JSON an `importers` array). Every
/// importer is a distinct file, so locating them scans each file's import rows once.
fn write_importers(s: &Session, q: &Query<'_>, w: &mut Writer<'_>, target: NodeId, imp: &Impact, limit: usize) -> RowCounts {
    let ix = q.index();
    let mut c = RowCounts::default();
    if w.json {
        w.open_array("importers");
    }
    // The target's modules (a def: its own module): their importers are direct.
    let modules = match (ix.kind(target).is_def(), ix.file(target)) {
        (true, Some(f)) => ix.file_node(f)..ix.file_node(f) + 1,
        (true, None) => target..target,
        (false, _) => ix.subtree(target),
    };
    for (&m, &via) in imp.importers.iter().zip(&imp.importer_via) {
        if !s.node_in_scope(m) {
            c.outside += 1;
            continue;
        }
        c.matched += 1;
        if c.shown >= limit {
            continue;
        }
        let site = timing::time("output.sites", || import_site(q, m, via));
        let rows = ix.file(m).map_or(0, |f| {
            let edges = q.imports().module_edges(f);
            edges.binary_search_by_key(&via, |e| e.dst).map_or(0, |i| edges[i].rows)
        });
        if w.json {
            w.item(c.shown);
            w.str("{");
            w.symbol_fields(m, true);
            w.key("imports", false);
            w.qid(via);
            w.key_bool("direct", modules.contains(&via));
            match site {
                Some((f, line, col)) => w.key_site("site", f, line, col),
                None => {
                    w.key("site", false);
                    w.str("null");
                }
            }
            w.key_num("rows", u64::from(rows));
            w.str("}");
        } else {
            match site {
                Some((f, line, col)) => w.loc(f, line, col),
                None => w.node_loc(m),
            }
            w.qid(m);
            w.str(" [importer] imports ");
            w.qid(via);
            if rows > 1 {
                w.str(" +");
                w.num(u64::from(rows - 1));
                w.str(if rows == 2 { " row" } else { " rows" });
            }
            w.end_line();
        }
        c.shown += 1;
    }
    if w.json {
        w.close_array(c.shown);
    }
    c
}

/// `impact of pkg.mod:f (function, pkg/mod.py:3): 5 affected (4 symbols, 1 module level),
/// 1 in tests, 2 files; 2 importing modules; most affected: pkg.a 3, pkg.b 2`.
fn summary_line(w: &mut Writer<'_>, target: u32, imp: &Impact, rows: &RowCounts, importers: &RowCounts) {
    w.str("impact of ");
    hops::describe_target(w, target);
    w.str(": ");
    w.num(imp.walk.hops.len() as u64);
    w.str(" affected (");
    w.num(u64::from(imp.symbols));
    w.str(if imp.symbols == 1 { " symbol, " } else { " symbols, " });
    w.num(u64::from(imp.module_scopes));
    w.str(" module level), ");
    w.num(u64::from(imp.tests));
    w.str(" in tests, ");
    w.num(u64::from(imp.files));
    w.str(if imp.files == 1 { " file; " } else { " files; " });
    w.num(imp.importers.len() as u64);
    w.str(if imp.importers.len() == 1 { " importing module" } else { " importing modules" });
    if !imp.modules.is_empty() {
        w.str("; most affected: ");
        for (i, &(m, n)) in imp.modules.iter().take(TOP_MODULES).enumerate() {
            if i > 0 {
                w.str(", ");
            }
            w.qid(m);
            w.str(" ");
            w.num(u64::from(n));
        }
    }
    let both = RowCounts {
        matched: rows.matched + importers.matched,
        shown: rows.shown + importers.shown,
        outside: rows.outside + importers.outside,
    };
    hops::filter_note(w, &both);
    w.end_line();
}
