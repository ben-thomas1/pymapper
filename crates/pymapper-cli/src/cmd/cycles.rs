//! `pymapper cycles` subcommand: import cycles of the project (or through one symbol's
//! modules), and with `--calls` the call cycle (recursion) through a symbol.
//!
//! ```text
//!   load (as check) ──> Query ──┬─ no --symbol: the cyclic components of the cycle import
//!                               │  graph (first-party runtime imports, as PM301)
//!                               ├─ --symbol: the components holding its modules
//!                               └─ --symbol --calls: the call SCC through it
//!   ──> PATHS filter (a cycle with a member under PATHS) ──> concise | json ──> exit 0
//! ```
//!
//! Every cycle is one line at the import (call) that starts its witness: a shortest cycle,
//! each member importing (calling) the next. Import witnesses start at the module whose import
//! closes the cycle, as PM301; call witnesses start at the symbol.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::index::NodeId;
use pymapper_core::query::{CycleHit, Query, Scratch, WalkOptions};
use pymapper_core::resolve::Confidence;
use pymapper_core::roots::Origin;
use pymapper_core::timing;

use super::query::hops::{self, RowCounts};
use super::query::{Pair, QueryFlags, Session, WalkFlags, Writer, finish, import_site, locate};

#[derive(clap::Args)]
pub struct Args {
    /// Show only cycles with a member in these files and directories (default: every cycle).
    /// The whole project is loaded either way; config and project-root discovery start at
    /// the first.
    paths: Vec<PathBuf>,
    /// Only cycles through this symbol's modules (a qualified id, dotted name or last
    /// segments, as for `callers`).
    #[arg(long, value_name = "SYMBOL")]
    symbol: Option<String>,
    /// The call cycle (direct or mutual recursion) through --symbol instead of import cycles.
    #[arg(long, requires = "symbol")]
    calls: bool,
    #[command(flatten)]
    walk: WalkFlags,
    #[command(flatten)]
    flags: QueryFlags,
}

/// What was searched.
enum Kind {
    Imports,
    Calls,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let s = Session::open("cycles", args.flags, args.paths)?;
    let q = s.query();
    let target = match &args.symbol {
        Some(text) => match s.resolve(&q, "cycles", text)? {
            Ok(v) => Some(v),
            Err(code) => return Ok(code),
        },
        None => None,
    };
    let mut scratch = Scratch::default();
    let walk = args.walk.walk(u32::MAX);
    let (kind, mut hits) = timing::time("query", || match target {
        Some(t) if args.calls => (Kind::Calls, q.call_cycle(t, &walk, &mut scratch).into_iter().collect()),
        Some(t) => (Kind::Imports, q.import_cycles(t, &mut scratch)),
        None => (Kind::Imports, all_import_cycles(&q, &mut scratch)),
    });
    let mut counts = RowCounts::default();
    hits.retain(|h| {
        let keep = h.members.iter().any(|&m| s.node_in_scope(m));
        counts.outside += usize::from(!keep);
        keep
    });
    counts.matched = hits.len();
    counts.shown = hits.len().min(s.row_limit);
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    timing::time("output", || write(&q, &mut w, &kind, target, &hits, &counts, &walk));
    finish(w)?;
    s.report_stats(&q, &[("cycles", counts.matched as u64)]);
    Ok(ExitCode::SUCCESS)
}

/// Every cyclic component of the cycle import graph, ordered by first member. The graph holds
/// first-party modules only: the components of every first-party group's subtree.
fn all_import_cycles(q: &Query<'_>, scratch: &mut Scratch) -> Vec<CycleHit> {
    let ix = q.index();
    let mut out: Vec<CycleHit> = Vec::new();
    for (g, group) in ix.groups().iter().enumerate() {
        if group.origin == Origin::FirstParty {
            out.extend(q.import_cycles(ix.group_node(g as u32), scratch));
        }
    }
    // A component spanning two groups is found from both.
    out.sort_by_key(|h| h.members[0]);
    out.dedup_by_key(|h| h.members[0]);
    out
}

/// The least sure edge of a call cycle witness under the walk's filters, and whether one of
/// its edges is only dispatch.
fn witness_edges(q: &Query<'_>, cycle: &[NodeId], walk: &WalkOptions) -> Vec<(Confidence, bool)> {
    let n = cycle.len();
    (0..n)
        .map(|i| {
            let (a, b) = (cycle[i], cycle[(i + 1) % n]);
            q.direct_callees(a)
                .filter(|&(w, c, d)| w == b && c <= walk.min_conf && (walk.overrides || !d))
                .map(|(_, c, d)| (c, d))
                .next()
                .unwrap_or((Confidence::Possible, false))
        })
        .collect()
}

fn write(q: &Query<'_>, w: &mut Writer<'_>, kind: &Kind, target: Option<NodeId>, hits: &[CycleHit], counts: &RowCounts, walk: &WalkOptions) {
    let calls = matches!(kind, Kind::Calls);
    if w.json {
        w.json_header("cycles");
        w.key_str("graph", if calls { "calls" } else { "imports" });
        w.key("target", false);
        match target {
            Some(t) => w.symbol_object(t),
            None => w.str("null"),
        }
        w.open_array("results");
    }
    let largest = hits.iter().map(|h| h.members.len()).max().unwrap_or(0);
    for (i, h) in hits[..counts.shown].iter().enumerate() {
        let wit = &h.witness;
        let edges = if calls { witness_edges(q, wit, walk) } else { Vec::new() };
        let site = match (calls, wit.len()) {
            (_, 0) => None,
            (true, n) => {
                let p = Pair { src: wit[0], dst: wit[1 % n], dispatch: edges[0].1 };
                locate(q, &[p])[0].first.map(|s| (s.file, s.line, s.col))
            }
            (false, n) => import_site(q, wit[0], wit[1 % n]),
        };
        let worst = edges.iter().map(|e| e.0).max();
        if w.json {
            w.item(i);
            w.str("{\"size\":");
            w.num(h.members.len() as u64);
            w.open_array("members");
            for (k, &m) in h.members.iter().enumerate() {
                if k > 0 {
                    w.str(",");
                }
                w.qid(m);
            }
            w.str("]");
            w.open_array("witness");
            for (k, &m) in wit.iter().enumerate() {
                if k > 0 {
                    w.str(",");
                }
                w.qid(m);
            }
            w.str("]");
            if let Some(c) = worst {
                w.key_str("worst", c.as_str());
            }
            match site {
                Some((f, line, col)) => w.key_site("site", f, line, col),
                None => {
                    w.key("site", false);
                    w.str("null");
                }
            }
            w.str("}");
            continue;
        }
        if let Some((f, line, col)) = site {
            w.loc(f, line, col);
        }
        w.str(if calls { "call cycle (" } else { "import cycle (" });
        w.num(h.members.len() as u64);
        w.str(match (calls, h.members.len() == 1) {
            (true, true) => " function): ",
            (true, false) => " functions): ",
            (false, true) => " module): ",
            (false, false) => " modules): ",
        });
        for &m in wit {
            w.qid(m);
            w.str(" -> ");
        }
        if let Some(&first) = wit.first() {
            w.qid(first);
        }
        if let Some(c) = worst {
            w.conf_tag(c, edges.iter().any(|e| e.1));
        }
        w.end_line();
    }
    if w.json {
        w.close_array(counts.shown);
        w.key("summary", false);
        w.str("{");
        w.key("cycles", true);
        w.num(counts.matched as u64);
        w.key_num("largest", largest as u64);
        hops::json_counts(w, counts);
        w.str("}");
        w.json_end();
        return;
    }
    let what = if calls { "call cycle" } else { "import cycle" };
    if counts.matched == 0 {
        w.str("no ");
        w.str(what);
        w.str("s");
    } else {
        w.num(counts.matched as u64);
        w.str(" ");
        w.str(what);
        if counts.matched != 1 {
            w.str("s");
        }
        if counts.matched > 1 && largest > 0 {
            w.str(" (largest: ");
            w.num(largest as u64);
            w.str(if calls { " functions)" } else { " modules)" });
        }
    }
    if let Some(t) = target {
        w.str(" through ");
        hops::describe_target(w, t);
    }
    hops::filter_note(w, counts);
    w.end_line();
}
