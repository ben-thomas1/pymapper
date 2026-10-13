//! `pymapper cycles` subcommand: import cycles of the project (or through one symbol's
//! modules), and with `--calls` the call cycle (recursion) through a symbol.
//!
//! ```text
//!   load (as check) ──> Query ──┬─ no --symbol: the cyclic components of the cycle import
//!                               │  graph (first-party runtime imports, as PM301)
//!                               ├─ --symbol: the components holding its modules
//!                               └─ --symbol --calls: the call SCC through it
//!   ──> PATHS filter (a cycle with a member under PATHS) ──> text | llm | json ──> exit 0
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
use super::query::{Pair, QueryFlags, Session, WalkFlags, Writer, finish, import_site, llm, locate};

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
    if w.llm {
        let arg = cmd_args(&args.symbol, args.calls);
        timing::time("output", || write_llm(&s, &q, &mut w, &kind, target, &hits, &counts, &walk, &arg));
    } else {
        timing::time("output", || write(&q, &mut w, &kind, target, &hits, &counts, &walk));
    }
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

/// One import statement from a module of a cycle to another member.
#[derive(Clone, Copy)]
pub struct CycleRow {
    pub src: NodeId,
    pub dst: NodeId,
    pub file: u32,
    pub line: u32,
    /// The row's [`res_flags`](pymapper_core::resolve::res_flags).
    pub flags: u8,
    /// Import row (store-absolute), for the imported names.
    pub row: usize,
}

/// Every import statement between two members of a cycle, by importing member (in `members`
/// order) then line. O(import rows of the members' files * log members).
pub fn cycle_rows(q: &Query<'_>, members: &[NodeId]) -> Vec<CycleRow> {
    let ix = q.index();
    let imp = q.imports();
    let st = &ix.facts().imports;
    let mut sorted = members.to_vec();
    sorted.sort_unstable();
    let mut out = Vec::new();
    for &m in members {
        let Some(f) = ix.file(m) else { continue };
        for row in ix.file_facts(f).imports.range() {
            let d = imp.module_node(row);
            if d != m && sorted.binary_search(&d).is_ok() {
                out.push(CycleRow { src: m, dst: d, file: f, line: st.line[row], flags: imp.flags(row), row });
            }
        }
    }
    out
}

/// Flags of the module edge `src -> dst` ([`res_flags::EDGE_ALL`] flags shared by every row:
/// none of the deferring ones means some row runs on import).
pub fn edge_flags(q: &Query<'_>, src: NodeId, dst: NodeId) -> u8 {
    let ix = q.index();
    let Some(f) = ix.file(src) else { return 0 };
    let edges = q.imports().module_edges(f);
    edges.binary_search_by_key(&dst, |e| e.dst).map_or(0, |i| edges[i].flags)
}

/// Whether import flags defer or guard an import (it does not run on import, or only for
/// type checkers).
pub fn deferred(flags: u8) -> bool {
    use pymapper_core::resolve::res_flags;
    flags & (res_flags::TYPE_ONLY | res_flags::CONDITIONAL | res_flags::TRY | res_flags::LAZY) != 0
}

/// The arguments echoed in the llm header.
fn cmd_args(symbol: &Option<String>, calls: bool) -> String {
    match (symbol, calls) {
        (Some(sym), true) => format!("--calls --symbol {sym}"),
        (Some(sym), false) => format!("--symbol {sym}"),
        (None, _) => String::new(),
    }
}

/// The llm rendering: one section per cycle with its witness and every witness edge at its
/// import (call) site with its kind (top-level / inside a function / TYPE_CHECKING / try /
/// conditional) or confidence; import cycles also count the other edges between members.
#[allow(clippy::too_many_arguments)]
fn write_llm(s: &Session, q: &Query<'_>, w: &mut Writer<'_>, kind: &Kind, target: Option<NodeId>, hits: &[CycleHit], counts: &RowCounts, walk: &WalkOptions, arg: &str) {
    let calls = matches!(kind, Kind::Calls);
    llm::header(w, "cycles", arg);
    if let Some(t) = target {
        llm::target(w, t);
        llm::sep(w);
    }
    llm::root(w, s);
    llm::sep(w);
    let largest = hits.iter().map(|h| h.members.len()).max().unwrap_or(0);
    llm::count(w, counts.matched as u64, if calls { "call cycle" } else { "import cycle" }, if calls { "call cycles" } else { "import cycles" });
    if counts.matched > 1 {
        w.str(" (largest: ");
        w.num(largest as u64);
        w.str(")");
    }
    w.end_line();
    if calls {
        w.str("A call cycle is direct or mutual recursion; tags are the edge confidence.");
        w.end_line();
        llm::call_blind_spots(w, s);
    } else {
        w.str("Graph: first-party imports");
        if s.config.analysis.type_checking_cycles {
            w.str(" (TYPE_CHECKING imports included)");
        } else {
            w.str(", TYPE_CHECKING imports excluded");
        }
        w.str(", as PM301. Kinds: top-level runs on import; inside a function, try and conditional imports are deferred or guarded, so a cycle closed only by them is usually harmless.");
        w.end_line();
        llm::import_blind_spots(w);
    }
    for (i, h) in hits[..counts.shown].iter().enumerate() {
        let wit = &h.witness;
        let n = wit.len();
        w.end_line();
        w.str("## Cycle ");
        w.num(i as u64 + 1);
        w.str(": ");
        llm::count(w, h.members.len() as u64, if calls { "function" } else { "module" }, if calls { "functions" } else { "modules" });
        let flags: Vec<u8> = if calls { Vec::new() } else { (0..n).map(|k| edge_flags(q, wit[k], wit[(k + 1) % n])).collect() };
        if !calls && n > 0 {
            w.str(if flags.iter().all(|&f| !deferred(f)) { ", eager (every edge runs on import)" } else { ", deferred (an edge does not run on import)" });
        }
        w.end_line();
        for &m in wit {
            w.qid(m);
            w.str(" -> ");
        }
        if let Some(&first) = wit.first() {
            w.qid(first);
        }
        w.end_line();
        if calls {
            let edges = witness_edges(q, wit, walk);
            let pairs: Vec<Pair> = (0..n).map(|k| Pair { src: wit[k], dst: wit[(k + 1) % n], dispatch: edges[k].1 }).collect();
            let sites = locate(q, &pairs);
            for (k, p) in pairs.iter().enumerate() {
                w.short(p.src);
                w.str(" -> ");
                w.short(p.dst);
                w.conf_tag(edges[k].0, edges[k].1);
                if let Some(site) = sites[k].first {
                    w.str(" at ");
                    w.loc_plain(site.file, site.line, site.col);
                }
                w.end_line();
            }
            continue;
        }
        for k in 0..n {
            let (a, b) = (wit[k], wit[(k + 1) % n]);
            w.qid(a);
            w.str(" -> ");
            w.qid(b);
            w.str(" (");
            w.import_kind(flags[k]);
            w.str(")");
            if let Some((f, line, col)) = import_site(q, a, b) {
                w.str(" at ");
                w.loc_plain(f, line, col);
            }
            w.end_line();
        }
        if h.members.len() > n {
            w.str("Also in the cycle: ");
            for (k, &m) in h.members.iter().filter(|m| !wit.contains(m)).enumerate() {
                if k > 0 {
                    w.str(", ");
                }
                w.qid(m);
            }
            w.end_line();
        }
        let rows = cycle_rows(q, &h.members);
        let mut pairs: Vec<(NodeId, NodeId)> = rows.iter().map(|r| (r.src, r.dst)).collect();
        pairs.sort_unstable();
        pairs.dedup();
        let other = pairs.len().saturating_sub(n);
        if other > 0 {
            llm::count(w, other as u64, "other import edge", "other import edges");
            w.str(" between members (`pymapper explain` on the PM301 id lists every one).");
            w.end_line();
        }
    }
    llm::trailer(w, counts.matched, counts.shown, counts.outside, "cycles");
    let next = if calls {
        "`pymapper callers <member>` to see who enters the recursion".to_owned()
    } else if counts.matched == 0 {
        "nothing to do; `pymapper cycles --calls --symbol <f>` finds recursion".to_owned()
    } else {
        "break an edge by moving a top-level import into the function that uses it, or move the shared names to a module both can import · `pymapper check --select PM301 --llm` for the finding ids".to_owned()
    };
    llm::next(w, &next);
}
