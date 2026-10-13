//! `pymapper callers` subcommand: who calls a symbol (and, with `--depth`, their callers).
//! Shares its implementation with `callees` ([`run_walk`]).
//!
//! ```text
//!   load (as check) ──> calls ──> Query ──> resolve SYMBOL ──> BFS over reverse (forward)
//!   call edges up to --depth ──> PATHS filter ──> sites of the shown rows ──> text | llm | json
//! ```

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::index::NodeId;
use pymapper_core::query::{Query, Scratch, Walk};
use pymapper_core::timing;

use super::query::hops::{self, Side, WalkCounts};
use super::query::{QueryFlags, Session, WalkFlags, Writer, finish, llm};

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
    if w.llm {
        let c = timing::time("output", || write_llm(&s, &q, &mut w, &walk, side, target, depth, &args.symbol));
        finish(w)?;
        s.report_stats(&q, &[("reached", walk.hops.len() as u64), ("shown", c.rows.shown as u64)]);
        return Ok(ExitCode::SUCCESS);
    }
    let counts = timing::time("output", || {
        if w.json {
            w.json_header(name);
            w.key("target", false);
            w.symbol_object(target);
            w.key("depth", false);
            if depth == u32::MAX { w.str("null") } else { w.num(u64::from(depth)) }
            w.open_array("results");
        }
        let counts = hops::write_rows(&s, &q, &mut w, &walk, side, s.row_limit, false);
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

/// The llm rendering of `callers` / `callees` (see `query::llm`).
#[allow(clippy::too_many_arguments)]
fn write_llm(s: &Session, q: &Query<'_>, w: &mut Writer<'_>, walk: &Walk, side: Side, target: NodeId, depth: u32, arg: &str) -> WalkCounts {
    let name = if side == Side::Callers { "callers" } else { "callees" };
    let c = hops::walk_counts(s, walk, s.row_limit, false);
    llm::header(w, name, arg);
    llm::target(w, target);
    llm::sep(w);
    llm::root(w, s);
    llm::sep(w);
    match side {
        Side::Callers => llm::count(w, c.rows.matched as u64, "caller", "callers"),
        Side::Callees => llm::count(w, c.rows.matched as u64, "callee", "callees"),
    }
    w.str(" in ");
    llm::count(w, c.files as u64, "file", "files");
    w.str(" (");
    w.num(c.possible as u64);
    w.str(" possible");
    if c.dispatch > 0 {
        w.str(", ");
        w.num(c.dispatch as u64);
        w.str(" dispatch only");
    }
    w.str(")");
    if depth == u32::MAX {
        w.str(" · transitive");
    } else if depth > 1 {
        w.str(" · up to depth ");
        w.num(u64::from(depth));
    }
    w.end_line();
    w.str("Confidence: certain/likely = resolved call; possible = guessed from a unique method name; dispatch = base method to an override.");
    w.end_line();
    llm::call_blind_spots(w, s);
    hops::write_llm_rows(s, q, w, walk, side, s.row_limit, false);
    llm::trailer(w, c.rows.matched, c.rows.shown, c.rows.outside, name);
    let id = llm::qid_arg(w, target);
    let next = match side {
        Side::Callers if c.rows.matched == 0 => format!(
            "nothing calls it statically; check dynamic use with `grep -rnw {} {} --include='*.py'` or `pymapper explain {id}`",
            short_name(w.ix, target),
            llm::root_arg(s)
        ),
        Side::Callers => format!("`pymapper impact {id}` for everything that depends on it transitively · `--depth N` for callers of callers"),
        Side::Callees => format!("`pymapper callees {id} --depth 2` to go deeper · `pymapper callers <callee>` for other users of a callee"),
    };
    llm::next(w, &next);
    c
}

/// The bare name of `v` (last segment of its qualified id, without a `#N` suffix).
pub fn short_name(ix: &pymapper_core::index::Index, v: NodeId) -> String {
    let id = ix.qid_string(v);
    let tail = id.rsplit([':', '.']).next().unwrap_or(&id);
    tail.split('#').next().unwrap_or(tail).to_owned()
}
