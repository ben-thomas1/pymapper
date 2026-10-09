//! `pymapper path` subcommand: a shortest call (or `--imports`: import) chain from one symbol
//! to another.
//!
//! ```text
//!   load (as check) ──> calls ──> Query ──> resolve FROM, TO ──> bidirectional BFS
//!   ──> one line per edge at its call site / import line ──> exit 0 found, 1 no path
//! ```
//!
//! There are no positional paths: a chain is one fact and is never filtered.

use std::io;
use std::process::ExitCode;

use pymapper_core::index::NodeId;
use pymapper_core::query::{PathGraph, PathOptions, Scope, Scratch, ShortestPath};
use pymapper_core::resolve::Confidence;
use pymapper_core::timing;

use super::query::hops;
use super::query::{EXIT_NOT_FOUND, Pair, QueryFlags, Session, Writer, finish, import_site, locate, parse_conf};

#[derive(clap::Args)]
pub struct Args {
    /// Where the chain starts (a qualified id, dotted name or last segments, as for `callers`).
    from: String,
    /// Where the chain ends.
    to: String,
    /// Follow module imports instead of calls (symbols stand for their modules).
    #[arg(long)]
    imports: bool,
    /// Least sure call edges followed: certain, likely or possible (every resolved edge).
    #[arg(long, value_name = "LEVEL", default_value = "possible", value_parser = parse_conf)]
    edges: Confidence,
    /// Do not follow override edges (a base method to its overrides: dynamic dispatch).
    #[arg(long)]
    no_overrides: bool,
    /// Treat classes, modules and packages as the node only, not everything in them.
    #[arg(long)]
    node: bool,
    /// Longest chain searched, in edges (default: unbounded).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_len: Option<u32>,
    #[command(flatten)]
    flags: QueryFlags,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let s = Session::open("path", args.flags, Vec::new())?;
    let q = s.query();
    let from = match s.resolve(&q, "path", &args.from)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let to = match s.resolve(&q, "path", &args.to)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let opts = PathOptions {
        graph: if args.imports { PathGraph::Imports } else { PathGraph::Calls },
        min_conf: args.edges,
        overrides: !args.no_overrides,
        max_len: args.max_len.unwrap_or(u32::MAX),
        scope: if args.node { Scope::Node } else { Scope::Auto },
    };
    let mut scratch = Scratch::default();
    let found = timing::time("query", || q.path(from, to, &opts, &mut scratch));
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    timing::time("output", || write(&q, &mut w, from, to, &opts, found.as_ref()));
    finish(w)?;
    s.report_stats(&q, &[("path_len", found.as_ref().map_or(0, |p| p.len() as u64))]);
    Ok(if found.is_some() { ExitCode::SUCCESS } else { ExitCode::from(EXIT_NOT_FOUND) })
}

/// Location of every edge of `p`: call edges at their first call site, import edges at
/// their first import row.
fn edge_sites(q: &pymapper_core::query::Query<'_>, p: &ShortestPath) -> Vec<Option<(u32, u32, u32)>> {
    if p.graph == PathGraph::Imports {
        return p.nodes.windows(2).map(|e| import_site(q, e[0], e[1])).collect();
    }
    let pairs: Vec<Pair> =
        p.nodes.windows(2).enumerate().map(|(i, e)| Pair { src: e[0], dst: e[1], dispatch: p.dispatch(i) }).collect();
    locate(q, &pairs).iter().map(|s| s.first.map(|x| (x.file, x.line, x.col))).collect()
}

fn write(q: &pymapper_core::query::Query<'_>, w: &mut Writer<'_>, from: NodeId, to: NodeId, opts: &PathOptions, found: Option<&ShortestPath>) {
    let what = if opts.graph == PathGraph::Imports { "import" } else { "call" };
    let sites = found.map(|p| edge_sites(q, p)).unwrap_or_default();
    if w.json {
        w.json_header("path");
        w.key_str("graph", if opts.graph == PathGraph::Imports { "imports" } else { "calls" });
        w.key("from", false);
        w.symbol_object(from);
        w.key("to", false);
        w.symbol_object(to);
        w.key_bool("found", found.is_some());
        let nodes: &[NodeId] = found.map_or(&[], |p| &p.nodes);
        w.open_array("nodes");
        for (i, &v) in nodes.iter().enumerate() {
            w.item(i);
            w.symbol_object(v);
        }
        w.close_array(nodes.len());
        w.open_array("edges");
        if let Some(p) = found {
            for i in 0..p.len() {
                w.item(i);
                w.str("{");
                w.key("from", true);
                w.qid(p.nodes[i]);
                w.key("to", false);
                w.qid(p.nodes[i + 1]);
                match p.conf(i) {
                    Some(c) => {
                        w.key_str("confidence", c.as_str());
                        w.key_bool("dispatch", p.dispatch(i));
                    }
                    None => w.import_flags(p.import_flags(i)),
                }
                match sites[i] {
                    Some((f, line, col)) => w.key_site("site", f, line, col),
                    None => {
                        w.key("site", false);
                        w.str("null");
                    }
                }
                w.str("}");
            }
        }
        w.close_array(found.map_or(0, |p| p.len()));
        w.key("summary", false);
        w.str("{");
        w.key("length", true);
        match found {
            Some(p) => w.num(p.len() as u64),
            None => w.str("null"),
        }
        match found.and_then(ShortestPath::worst) {
            Some(c) => w.key_str("worst", c.as_str()),
            None => {
                w.key("worst", false);
                w.str("null");
            }
        }
        w.str("}");
        w.json_end();
        return;
    }
    if let Some(p) = found {
        for i in 0..p.len() {
            match sites[i] {
                Some((f, line, col)) => w.loc(f, line, col),
                None => w.node_loc(p.nodes[i]),
            }
            w.qid(p.nodes[i]);
            w.str(" -> ");
            w.qid(p.nodes[i + 1]);
            match p.conf(i) {
                Some(c) => w.conf_tag(c, p.dispatch(i)),
                None => {
                    w.str(" [import");
                    w.import_flags(p.import_flags(i));
                    w.str("]");
                }
            }
            w.end_line();
        }
    }
    match found {
        Some(p) => {
            w.str(what);
            w.str(" path from ");
            hops::describe_target(w, from);
            w.str(" to ");
            hops::describe_target(w, to);
            w.str(": ");
            w.num(p.len() as u64);
            w.str(if p.len() == 1 { " edge" } else { " edges" });
            if let Some(c) = p.worst() {
                w.str(", least sure ");
                w.str(c.as_str());
            }
            if p.is_empty() {
                w.str(" (the symbols overlap)");
            }
        }
        None => {
            w.str("no ");
            w.str(what);
            w.str(" path from ");
            hops::describe_target(w, from);
            w.str(" to ");
            hops::describe_target(w, to);
            if opts.max_len != u32::MAX {
                w.str(" within ");
                w.num(u64::from(opts.max_len));
                w.str(" edges");
            }
        }
    }
    w.end_line();
}
