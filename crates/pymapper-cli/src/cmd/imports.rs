//! `pymapper imports` subcommand: module-level import dependencies of a symbol, outgoing
//! (what it imports) and incoming (what imports it), rolled up to packages with `--depth`.
//!
//! ```text
//!   load (as check) ──> Query ──> resolve SYMBOL ──> Query::deps (imports only, both
//!   directions; other endpoint mapped to its module, or its ancestor at --depth) ──> PATHS
//!   filter ──> Query::import_sites (shown rows) ──> concise | json
//! ```
//!
//! Every row sits at the first import statement behind it (the target's files for `out`, the
//! importer's for `in`), as `callers` rows sit at their first call site.
//!
//! A package or module target covers every module below it; a class or function covers the
//! import statements inside it (outgoing only: imports name modules). Representatives never
//! contain the target: a sibling rolled up to the shared parent package stays the sibling's
//! shallowest ancestor outside the target's ancestry.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pymapper_core::graph::CallFilter;
use pymapper_core::index::{Index, NodeId};
use pymapper_core::query::{DepOptions, Deps, Direction, ExternalDep, ImportSite, Query, Scratch};
use pymapper_core::resolve::{Confidence, ExtKind, res_flags};
use pymapper_core::rollup::Level;
use pymapper_core::timing;

use super::query::hops::{self, RowCounts};
use super::query::{QueryFlags, Session, Writer, finish, origin_str};

#[derive(clap::Args)]
pub struct Args {
    /// The symbol: a package or module (pkg.sub, pkg.mod), or a class or function for the
    /// imports inside it; qualified id, dotted name or last segments as for `callers`.
    symbol: String,
    /// Show only modules in these files and directories (default: everything). The whole
    /// project is loaded either way; config and project-root discovery start at the first.
    paths: Vec<PathBuf>,
    /// Roll the other side up to packages: keep the first N dotted components (1: top-level
    /// packages). Default: modules.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    depth: Option<u32>,
    /// Only one direction: out (what the symbol imports) or in (what imports it).
    #[arg(long, value_name = "DIR", value_parser = parse_direction)]
    direction: Option<Direction>,
    /// Skip imports only type checkers see (`if TYPE_CHECKING:` blocks).
    #[arg(long)]
    runtime: bool,
    /// Rows printed per direction (heaviest first); 0 prints every row.
    #[arg(long, value_name = "N", default_value_t = 200)]
    limit: usize,
    #[command(flatten)]
    flags: QueryFlags,
}

fn parse_direction(s: &str) -> Result<Direction, String> {
    match s {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        _ => Err(format!("`{s}` is not a direction (expected out or in)")),
    }
}

/// Rows of one direction, before output.
struct Side {
    deps: Deps,
    counts: RowCounts,
    /// Import statement of each shown row of `deps.deps` / `deps.external` (parallel).
    sites: Vec<Option<ImportSite>>,
    ext_sites: Vec<Option<ImportSite>>,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let s = Session::open("imports", args.flags, args.paths)?;
    let q = s.query();
    let target = match s.resolve(&q, "imports", &args.symbol)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    // Imports name modules: nothing imports a class or function.
    if args.direction == Some(Direction::In) && q.index().kind(target).is_def() {
        anyhow::bail!(
            "nothing imports a {} (imports name modules): `--direction in` needs a package or module, such as `{}`",
            q.index().kind(target).as_str(),
            q.index().qid_string(q.index().file(target).map_or(target, |f| q.index().file_node(f)))
        );
    }
    let opts = DepOptions {
        level: args.depth.map_or(Level::Module, Level::Depth),
        // Imports only: a filter that keeps no call edge.
        calls: CallFilter { threshold: Confidence::Certain, exclude: u64::MAX },
        imports: true,
        import_exclude: if args.runtime { res_flags::TYPE_ONLY } else { 0 },
    };
    let limit = if args.limit == 0 { usize::MAX } else { args.limit };
    let mut scratch = Scratch::default();
    let mut side = |dir: Direction| -> Option<Side> {
        // Imports name modules: nothing imports a class or function.
        if args.direction.is_some_and(|d| d != dir) || (dir == Direction::In && q.index().kind(target).is_def()) {
            return None;
        }
        let mut deps = timing::time("query", || q.deps(target, dir, &opts, &mut scratch));
        deps.deps.retain(|d| d.imports > 0);
        // Most rows first, then by name (interned ids depend on scheduling).
        let ix = q.index();
        deps.external.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| ext_key(ix, &s, a).cmp(&ext_key(ix, &s, b))));
        // Imports of code that is not loaded have no module file to filter by: shown only
        // without a filter.
        let outside_ext = if s.filtered() { std::mem::take(&mut deps.external).len() } else { 0 };
        let mut counts = RowCounts { outside: outside_ext, ..RowCounts::default() };
        deps.deps.retain(|d| {
            let keep = s.node_in_scope(d.node);
            counts.outside += usize::from(!keep);
            keep
        });
        counts.matched = deps.deps.len() + deps.external.len();
        counts.shown = counts.matched.min(limit);
        // Rows are written modules first, then imports of code that is not loaded.
        let n = deps.deps.len().min(limit);
        let n_ext = deps.external.len().min(limit - n);
        let (sites, ext_sites) = timing::time("output.sites", || {
            q.import_sites(target, dir, &opts, &deps.deps[..n], &deps.external[..n_ext], &mut scratch)
        });
        Some(Side { deps, counts, sites, ext_sites })
    };
    let out = side(Direction::Out);
    let inc = side(Direction::In);
    let mut stdout = io::stdout().lock();
    let mut w = s.writer(&mut stdout);
    timing::time("output", || write(&q, &mut w, target, args.depth, out.as_ref(), inc.as_ref(), limit));
    finish(w)?;
    let n = |x: &Option<Side>| x.as_ref().map_or(0, |x| x.counts.matched as u64);
    s.report_stats(&q, &[("out", n(&out)), ("in", n(&inc))]);
    Ok(ExitCode::SUCCESS)
}

/// Sort key of an import of code that is not loaded: kind, then name.
fn ext_key<'a>(ix: &'a Index, s: &'a Session, e: &ExternalDep) -> (ExtKind, &'a [u8]) {
    let name = match e.dist() {
        Some(d) => s.project.roots.dists.name(d),
        None => e.name(ix).unwrap_or(b""),
    };
    (e.kind, name)
}

fn ext_origin(e: &ExternalDep) -> &'static str {
    match e.kind {
        ExtKind::Stdlib => "stdlib",
        ExtKind::ThirdParty => "third-party",
        ExtKind::Unknown => "unknown",
    }
}

/// `,"site":{"path":..,"line":..,"col":..}` or `,"site":null`.
fn json_site(w: &mut Writer<'_>, site: Option<ImportSite>) {
    match site {
        Some(st) => w.key_site("site", st.file, st.line, st.col),
        None => {
            w.key("site", false);
            w.str("null");
        }
    }
}

fn write(q: &Query<'_>, w: &mut Writer<'_>, target: NodeId, depth: Option<u32>, out: Option<&Side>, inc: Option<&Side>, limit: usize) {
    let ix = q.index();
    if w.json {
        w.json_header("imports");
        w.key("target", false);
        w.symbol_object(target);
        w.key("depth", false);
        match depth {
            Some(d) => w.num(u64::from(d)),
            None => w.str("null"),
        }
        w.open_array("results");
    }
    let mut i = 0;
    for (dir, side) in [("out", out), ("in", inc)] {
        let Some(side) = side else { continue };
        let mut n = 0;
        for (d, site) in side.deps.deps.iter().zip(&side.sites) {
            if n >= limit {
                break;
            }
            n += 1;
            if w.json {
                w.item(i);
                w.str("{");
                w.key("direction", true);
                w.json_str(dir);
                w.symbol_fields(d.node, false);
                w.key_str("origin", origin_str(ix, d.node));
                w.key_num("rows", u64::from(d.imports));
                w.import_flags(d.import_flags);
                json_site(w, *site);
                w.str("}");
            } else {
                // At the import statement; without one (never expected), at the module.
                match site {
                    Some(st) => w.loc(st.file, st.line, st.col),
                    None => w.node_loc(d.node),
                }
                w.str(if dir == "out" { "imports " } else { "imported by " });
                w.qid(d.node);
                w.str(" (");
                let o = origin_str(ix, d.node);
                if o != "first-party" {
                    w.str(o);
                    w.str(", ");
                }
                w.num(u64::from(d.imports));
                w.str(if d.imports == 1 { " row" } else { " rows" });
                w.import_flags(d.import_flags);
                w.str(")");
                // Where the imported module is (the importer is where the statement is).
                if dir == "out"
                    && site.is_some()
                    && let Some(f) = ix.file(d.node)
                {
                    w.str(" def ");
                    w.file(f);
                }
                w.end_line();
            }
            i += 1;
        }
        for (e, site) in side.deps.external.iter().zip(&side.ext_sites) {
            if n >= limit {
                break;
            }
            n += 1;
            if w.json {
                w.item(i);
                w.str("{");
                w.key("direction", true);
                w.json_str(dir);
                w.key("external", false);
                w.external_name(e);
                w.key_str("origin", ext_origin(e));
                w.key_num("rows", u64::from(e.rows));
                json_site(w, *site);
                w.str("}");
            } else {
                if let Some(st) = site {
                    w.loc(st.file, st.line, st.col);
                }
                w.str("imports ");
                w.external_name(e);
                w.str(" (");
                w.str(ext_origin(e));
                w.str(", not loaded, ");
                w.num(u64::from(e.rows));
                w.str(if e.rows == 1 { " row)" } else { " rows)" });
                w.end_line();
            }
            i += 1;
        }
    }
    let count = |s: Option<&Side>| s.map_or(0, |s| s.counts.matched as u64);
    let merged = RowCounts {
        matched: out.map_or(0, |s| s.counts.matched) + inc.map_or(0, |s| s.counts.matched),
        shown: out.map_or(0, |s| s.counts.shown) + inc.map_or(0, |s| s.counts.shown),
        outside: out.map_or(0, |s| s.counts.outside) + inc.map_or(0, |s| s.counts.outside),
    };
    if w.json {
        w.close_array(i);
        w.key("summary", false);
        w.str("{");
        w.key("out", true);
        match out {
            Some(s) => w.num(s.counts.matched as u64),
            None => w.str("null"),
        }
        w.key("external", false);
        match out {
            Some(s) => w.num(s.deps.external.len() as u64),
            None => w.str("null"),
        }
        w.key("in", false);
        match inc {
            Some(s) => w.num(s.counts.matched as u64),
            None => w.str("null"),
        }
        hops::json_counts(w, &merged);
        w.str("}");
        w.json_end();
        return;
    }
    w.str("imports of ");
    hops::describe_target(w, target);
    if let Some(d) = depth {
        w.str(" at depth ");
        w.num(u64::from(d));
    }
    w.str(":");
    if let Some(s) = out {
        w.str(" ");
        w.num(count(out));
        w.str(" out");
        if !s.deps.external.is_empty() {
            w.str(" (");
            w.num(s.deps.external.len() as u64);
            w.str(" not loaded)");
        }
    }
    if inc.is_some() {
        w.str(if out.is_some() { ", " } else { " " });
        w.num(count(inc));
        w.str(" in");
    }
    hops::filter_note(w, &merged);
    w.end_line();
}
