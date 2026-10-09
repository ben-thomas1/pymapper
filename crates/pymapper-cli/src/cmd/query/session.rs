//! Loading for the query subcommands: flags, the whole-program load and symbol resolution.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use pymapper_core::config::{Config, OutputFormat};
use pymapper_core::analysis::cycles::CycleOptions;
use pymapper_core::index::{FileId, NodeId};
use pymapper_core::query::{LookupOptions, Match, Query, QueryOptions, Resolved, Scope, WalkOptions};
use pymapper_core::resolve::{Confidence, ResolvedCalls, resolve_calls};
use pymapper_core::timing;

use super::render::{Writer, finish};
use crate::cmd::config::ConfigArgs;
use crate::pipeline::{self, Project, warn};

/// Flags shared by every query subcommand.
#[derive(clap::Args)]
pub struct QueryFlags {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// Print phase timings, counters and peak RSS to stderr.
    #[arg(long)]
    pub stats: bool,
}

/// Flags of the call graph walks (`callers`, `callees`, `impact`).
#[derive(clap::Args, Clone, Copy)]
pub struct WalkFlags {
    /// Least sure call edges followed: certain, likely or possible (every resolved edge).
    #[arg(long, value_name = "LEVEL", default_value = "possible", value_parser = parse_conf)]
    pub edges: Confidence,
    /// Do not follow override edges (a base method to its overrides: dynamic dispatch).
    #[arg(long)]
    pub no_overrides: bool,
    /// What the symbol covers: auto (a function itself, anything else its whole subtree),
    /// node (the node only) or subtree.
    #[arg(long, value_name = "SCOPE", default_value = "auto", value_parser = parse_scope)]
    pub scope: Scope,
    /// Rows printed (closest first); 0 prints every row.
    #[arg(long, value_name = "N", default_value_t = 200)]
    pub limit: usize,
}

impl WalkFlags {
    /// Walk options for `depth` hops (no node limit: `--limit` bounds the rows printed).
    pub fn walk(&self, depth: u32) -> WalkOptions {
        WalkOptions { depth, min_conf: self.edges, overrides: !self.no_overrides, scope: self.scope, limit: usize::MAX }
    }

    /// The row limit (`usize::MAX` for 0).
    pub fn row_limit(&self) -> usize {
        if self.limit == 0 { usize::MAX } else { self.limit }
    }
}

/// Parses a call edge confidence (`certain`, `likely`, `possible`).
pub fn parse_conf(s: &str) -> Result<Confidence, String> {
    match s {
        "certain" => Ok(Confidence::Certain),
        "likely" => Ok(Confidence::Likely),
        "possible" => Ok(Confidence::Possible),
        _ => Err(format!("`{s}` is not a confidence level (expected certain, likely or possible)")),
    }
}

fn parse_scope(s: &str) -> Result<Scope, String> {
    match s {
        "auto" => Ok(Scope::Auto),
        "node" => Ok(Scope::Node),
        "subtree" => Ok(Scope::Subtree),
        _ => Err(format!("`{s}` is not a scope (expected auto, node or subtree)")),
    }
}

/// A loaded program, ready for queries.
pub struct Session {
    t0: Instant,
    stats: bool,
    pub config: Config,
    pub root: PathBuf,
    pub project: Project,
    pub calls: ResolvedCalls,
    pub json: bool,
    /// Per file: under the positional paths (`None`: no filter).
    in_scope: Option<Vec<bool>>,
}

impl Session {
    /// Loads the configuration and the whole program exactly as `check` does (`paths` filter
    /// the output only), then resolves calls. Configuration errors, paths that do not exist
    /// and the `sarif` / `github` formats are usage errors.
    pub fn open(name: &str, flags: QueryFlags, paths: Vec<PathBuf>) -> anyhow::Result<Session> {
        let t0 = Instant::now();
        if flags.stats {
            timing::enable(true);
        }
        let explicit = !paths.is_empty();
        let loaded = timing::time("config", || flags.config.load(paths))?;
        let config_file = loaded.project.config_file.as_ref().map(|(p, _)| p.as_path());
        pipeline::require_paths(&loaded.scope, true, None)?;
        pipeline::require_paths(&loaded.config.paths, explicit, config_file)?;
        let json = match loaded.config.output_format {
            OutputFormat::Concise => false,
            OutputFormat::Json => true,
            other => anyhow::bail!("`pymapper {name}` writes the concise and json formats, not `{}`", other.as_str()),
        };
        let root = loaded.project.root;
        let mut scope = loaded.scope;
        for p in scope.iter().filter(|p| !p.starts_with(&root)) {
            warn(format_args!(
                "{} is outside the project root {} and is not loaded (run pymapper on it separately)",
                p.display(),
                root.display()
            ));
        }
        // A path covering the whole project (`.` at the root) filters nothing.
        if scope.iter().any(|s| root.starts_with(s)) {
            scope.clear();
        }
        let config = loaded.config;
        pipeline::configure_threads(config.threads)?;
        let project = pipeline::load(&config, &root, None);
        let calls = resolve_calls(&project.index, &project.imports);
        let in_scope: Option<Vec<bool>> = (!scope.is_empty()).then(|| {
            let ix = &project.index;
            (0..ix.file_count()).map(|f| scope.iter().any(|s| ix.file_path(f).starts_with(s))).collect()
        });
        if in_scope.as_ref().is_some_and(|s| !s.contains(&true)) {
            let mut list = String::new();
            for (i, p) in scope.iter().enumerate() {
                if i > 0 {
                    list.push_str(", ");
                }
                list.push_str(&p.to_string_lossy());
            }
            warn(format_args!("no loaded Python files under {list}: every row is filtered out"));
        }
        Ok(Session { t0, stats: flags.stats, config, root, project, calls, json, in_scope })
    }

    /// The query state (call graph built here, O(V + E)).
    pub fn query(&self) -> Query<'_> {
        let opts = QueryOptions { cycles: CycleOptions::from_config(&self.config) };
        timing::time("query.build", || Query::with_options(&self.project.index, &self.project.imports, &self.calls, opts))
    }

    /// Whether positional paths filter the output.
    #[inline]
    pub fn filtered(&self) -> bool {
        self.in_scope.is_some()
    }

    /// Whether file `f` lies under the positional paths (always without paths).
    #[inline]
    pub fn file_in_scope(&self, f: FileId) -> bool {
        self.in_scope.as_ref().is_none_or(|s| s[f as usize])
    }

    /// Whether node `v` is shown under the positional paths: its file is in scope, or for a
    /// node without a file (namespace package, distribution), some file below it is.
    pub fn node_in_scope(&self, v: NodeId) -> bool {
        let Some(s) = &self.in_scope else { return true };
        let ix = &self.project.index;
        if let Some(f) = ix.file(v) {
            return s[f as usize];
        }
        // The packages and modules below `v`, jumping over def subtrees.
        let end = ix.subtree(v).end;
        let mut w = v;
        while w < end {
            if ix.kind(w).is_def() {
                w = ix.subtree(w).end;
                continue;
            }
            if ix.file(w).is_some_and(|f| s[f as usize]) {
                return true;
            }
            w += 1;
        }
        false
    }

    /// A writer over stdout for this session's format.
    pub fn writer<'w>(&'w self, sink: &'w mut dyn Write) -> Writer<'w> {
        Writer::new(&self.project.index, &self.root, &self.project.roots, self.json, sink)
    }

    /// Resolves the symbol argument `text`. On an ambiguous or unknown symbol, writes the
    /// candidates (stdout) and the error (stderr) and returns the exit code to end with.
    pub fn resolve(&self, q: &Query<'_>, name: &str, text: &str) -> anyhow::Result<Result<NodeId, ExitCode>> {
        let res = timing::time("lookup", || q.resolve(text.as_bytes(), &LookupOptions::default()));
        let (error, matches) = match res {
            Resolved::Unique(v) => return Ok(Ok(v)),
            Resolved::Ambiguous(m) => ("ambiguous", m),
            Resolved::NotFound(m) => ("not-found", m),
        };
        let mut stdout = io::stdout().lock();
        let mut w = self.writer(&mut stdout);
        w.candidates(name, text, error, &matches);
        finish(w)?;
        let mut err = io::stderr().lock();
        // Ignore write failures: stderr may be closed and there is nowhere else to report.
        let _ = if error == "ambiguous" {
            writeln!(
                err,
                "pymapper: error: `{text}` is ambiguous: {} matches{} (listed on stdout); pass a qualified id such as `{}`",
                matches.len(),
                if matches.len() >= LookupOptions::default().limit { " or more" } else { "" },
                Self::example(q, &matches)
            )
        } else if matches.is_empty() {
            writeln!(err, "pymapper: error: no symbol matches `{text}`")
        } else {
            writeln!(err, "pymapper: error: no symbol matches `{text}`; {} similar name(s) listed on stdout", matches.len())
        };
        self.report_stats(q, &[("candidates", matches.len() as u64)]);
        Ok(Err(ExitCode::from(crate::EXIT_ERROR)))
    }

    fn example(q: &Query<'_>, m: &[Match]) -> String {
        m.first().map_or_else(String::new, |m| q.index().qid_string(m.node))
    }

    /// Prints `--stats` (phases, `counters`, standard counters, peak RSS) when requested.
    pub fn report_stats(&self, q: &Query<'_>, counters: &[(&str, u64)]) {
        if !self.stats {
            return;
        }
        let ix = &self.project.index;
        let s = ix.stats();
        let mut all = vec![
            ("files", s.files),
            ("loc", s.lines),
            ("nodes", u64::from(ix.node_count())),
            ("call_edges", self.calls.edges().len() as u64),
            ("query_kb", (q.heap_bytes() / 1000) as u64),
            ("threads", rayon::current_num_threads() as u64),
        ];
        all.extend_from_slice(counters);
        if let Some(cs) = &self.project.cache {
            all.extend([("cache_hits", cs.hits + cs.verified), ("cache_misses", cs.misses)]);
        }
        crate::stats::report(self.t0.elapsed(), &all);
    }
}
