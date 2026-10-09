//! `pymapper callers` subcommand: who calls a symbol (and, with `--depth`, their callers).
//! Shares its implementation with `callees` ([`run_walk`]).
//!
//! ```text
//!   load (as check) ──> calls ──> Query ──> resolve SYMBOL ──> BFS over reverse (forward)
//!   call edges up to --depth ──> PATHS filter ──> sites of the shown rows ──> concise | json
//! ```

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::query::Scratch;
use pymapper_core::timing;

use super::query::hops::{self, Side};
use super::query::{QueryFlags, Session, WalkFlags, finish};

#[derive(clap::Args)]
pub struct Args {
    /// The symbol: a qualified id (pkg.mod:Class.method), a dotted name (pkg.mod.Class.method)
    /// or its last segments (Class.method, method). A class, module or package stands for
    /// everything in it (see --scope).
    symbol: String,
    /// Show only rows in these files and directories (default: everything). The whole project
    /// is loaded either way; config and project-root discovery start at the first.
    paths: Vec<PathBuf>,
    /// Hops from the symbol (1: direct only).
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    depth: u32,
    /// Follow every hop (overrides --depth).
    #[arg(long)]
    transitive: bool,
    #[command(flatten)]
    walk: WalkFlags,
    #[command(flatten)]
    flags: QueryFlags,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    run_walk(args, Side::Callers)
}

/// `callers` / `callees`: one walk, rows, a summary line.
pub fn run_walk(args: Args, side: Side) -> anyhow::Result<ExitCode> {
    let name = if side == Side::Callers { "callers" } else { "callees" };
    let s = Session::open(name, args.flags, args.paths)?;
    let q = s.query();
    let target = match s.resolve(&q, name, &args.symbol)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let depth = if args.transitive { u32::MAX } else { args.depth };
    let opts = args.walk.walk(depth);
    let mut scratch = Scratch::default();
    let walk = timing::time("query", || match side {
        Side::Callers => q.callers(target, &opts, &mut scratch),
        Side::Callees => q.callees(target, &opts, &mut scratch),
    });
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    let counts = timing::time("output", || {
        if w.json {
            w.json_header(name);
            w.key("target", false);
            w.symbol_object(target);
            w.key("depth", false);
            if depth == u32::MAX { w.str("null") } else { w.num(u64::from(depth)) }
            w.open_array("results");
        }
        let counts = hops::write_rows(&s, &q, &mut w, &walk, side, args.walk.row_limit(), false);
        if w.json {
            w.close_array(counts.shown);
            w.key("summary", false);
            w.str("{");
            w.key("results", true);
            w.num(counts.matched as u64);
            hops::json_counts(&mut w, &counts);
            w.str("}");
            w.json_end();
        } else {
            w.num(counts.matched as u64);
            w.str(" ");
            w.str(match (side, counts.matched == 1) {
                (Side::Callers, true) => "caller of ",
                (Side::Callers, false) => "callers of ",
                (Side::Callees, true) => "callee of ",
                (Side::Callees, false) => "callees of ",
            });
            hops::describe_target(&mut w, target);
            if depth == u32::MAX {
                w.str(", transitive");
            } else if depth > 1 {
                w.str(", up to depth ");
                w.num(u64::from(depth));
            }
            hops::filter_note(&mut w, &counts);
            w.end_line();
        }
        counts
    });
    finish(w)?;
    s.report_stats(&q, &[("reached", walk.hops.len() as u64), ("shown", counts.shown as u64)]);
    Ok(ExitCode::SUCCESS)
}
