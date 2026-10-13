//! `pymapper deps --check`: the dependency-hygiene rules PM601-PM604, reported like `check`.
//!
//! ```text
//!   config ──> roots ──> walk ──> index (cache) ──> imports        (third-party code only with --deps)
//!     ──> declarations: every unit's pyproject.toml (dependencies, groups, # pymapper: ignore)
//!     ──> hygiene: per first-party import row ──> PM601 / PM603 / PM604; per unit ──> PM602
//!     ──> scope (positional PATHS) ──> filter (selection, per-file-ignores, suppressions,
//!         confidence) ──> report ──> baseline ──> output ──> exit 0 clean / 1 findings / 2 error
//! ```
//!
//! Same formats, suppressions, `per-file-ignores`, baselines and exit codes as `check`.
//! Findings about a declared dependency are located in the `pyproject.toml` that declares it;
//! suppress them with a `# pymapper: ignore[PM602]` comment on its line, or `per-file-ignores`
//! on `pyproject.toml`.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use pymapper_core::config::{Loaded, OutputFormat};
use pymapper_core::deps::{self, DeclStatus, Declarations, HygieneContext, HygieneCtx, HygieneRules};
use pymapper_core::index::NONE;
use pymapper_core::intern::Interner;
use pymapper_core::output::{self, IndexContext, Options, Outcome, Report, RunNotes, Summary};
use pymapper_core::rules::{Findings, RuleEngine};
use pymapper_core::timing;

use crate::cmd::check::plan::check_selectors;
use crate::cmd::check::{
    RelPath, compile_per_file_ignores, display_rel, file_scope, first_party_files, per_file_matches, per_file_path_matches,
    read_baseline, save_baseline, warn_bad_code,
};
use crate::pipeline::{self, warn};

/// The `--check` options.
pub struct Opts {
    pub baseline: Option<PathBuf>,
    pub save_baseline: Option<PathBuf>,
    pub stats: bool,
}

/// Runs the hygiene rules on the project of `loaded` (paths already validated).
pub fn run(mut loaded: Loaded, opts: Opts, t0: Instant) -> anyhow::Result<ExitCode> {
    let cwd = std::env::current_dir().context("cannot determine the working directory")?;
    if let Some(b) = opts.baseline {
        loaded.config.baseline = Some(cwd.join(b));
    }
    let config = &loaded.config;
    let root = &loaded.project.root;
    let scope = &loaded.scope;
    for p in scope.iter().filter(|p| !p.starts_with(root)) {
        warn(format_args!(
            "{} is outside the project root {} and is not checked (run pymapper on it separately)",
            p.display(),
            root.display()
        ));
    }
    let scope: &[PathBuf] = if scope.iter().any(|s| root.starts_with(s)) { &[] } else { scope };
    pipeline::configure_threads(config.threads)?;
    check_selectors(config)?;
    let engine = RuleEngine::new(config);
    let rules = HygieneRules::from_engine(&engine);
    if rules == HygieneRules::default() {
        anyhow::bail!(
            "none of the selected rules is a dependency rule: `pymapper deps --check` runs PM601-PM604 \
             (select them with `--select PM6`); the other rules are run by `pymapper check`"
        );
    }
    let per_file = compile_per_file_ignores(config)?;
    let baseline = match (&opts.save_baseline, &config.baseline) {
        (None, Some(path)) => Some(read_baseline(path)?),
        _ => None,
    };

    let project = pipeline::load(config, root, None);
    let ix = &project.index;
    let mut names = Interner::new();
    let decls = timing::time("deps.declarations", || Declarations::load(&project.units, ix.file_count(), &mut names));
    warn_declarations(&decls, root, rules, ix.file_count() > 0);
    if project.roots.venv.is_none() {
        warn(format_args!(
            "no virtualenv found: imports are mapped to distributions by name only (confidence 60-90); \
             create one or pass --venv for metadata-exact mapping"
        ));
    }
    let cx = HygieneCtx { ix, imp: &project.imports, dists: &project.roots.dists, units: &project.units, decls: &decls };
    let mut findings = Findings::new();
    let hs = timing::time("deps.hygiene", || deps::hygiene(&cx, rules, &mut names, &mut findings));

    // Report only what lies under the positional paths: Python files by the index scope,
    // findings in a pyproject.toml by its path.
    let in_scope = file_scope(ix, scope);
    let (files_checked, files_total) = first_party_files(ix, in_scope.as_deref());
    let pyproject_in_scope = |u: u32| scope.is_empty() || scope.iter().any(|s| decls.unit(u).path.starts_with(s));
    if let Some(sc) = &in_scope {
        findings.retain(|f, _| match decls.unit_of_file(f.loc.file) {
            Some(u) => pyproject_in_scope(u),
            None => f.loc.file.0 < ix.file_count() && sc[f.loc.file.0 as usize],
        });
    }
    let suppressions = timing::time("suppressions", || {
        let mut b = pipeline::suppressions_builder(ix, |_, _, _, _| {});
        for u in 0..decls.len() {
            let d = decls.unit(u);
            b.push_file(decls.file_id(u), &d.suppressions);
            if pyproject_in_scope(u) {
                for (line, code, kind) in &d.bad_suppressions {
                    warn_bad_code(display_rel(&d.path, root), *line, code, *kind);
                }
            }
        }
        b.build()
    });
    let filter = timing::time("filter", || {
        engine.filter(&mut findings, &suppressions, false, |f, i| match decls.unit_of_file(f) {
            Some(u) => per_file_path_matches(&decls.unit(u).path, root, &per_file[i]),
            None => per_file_matches(ix, root, &per_file[i], f),
        })
    });

    // Text formats show paths relative to the working directory; json and sarif keep the
    // project root.
    let text = !matches!(config.output_format, OutputFormat::Json | OutputFormat::Sarif);
    let mut ctx = HygieneContext::new(IndexContext::new(ix, root, &config.contracts), &names, &decls, root);
    if text {
        ctx = ctx.relative_to(&cwd);
    }
    let mut report = timing::time("report", || Report::new(&findings, &ctx));
    let status = if let Some(path) = &opts.save_baseline {
        let path = cwd.join(path);
        save_baseline(&report, &path)?;
        let _ = writeln!(io::stderr().lock(), "pymapper: saved {} finding(s) to baseline {}", report.len(), path.display());
        output::ExitStatus::Clean
    } else {
        if let Some(b) = &baseline {
            report.apply_baseline(b);
        }
        let opts = Options::from_config(config, io::stdout().is_terminal());
        let llm = config.output_format == OutputFormat::Llm;
        let root_rel = if llm { RelPath(root, &cwd).to_string() } else { String::new() };
        let filter_note = if llm && !scope.is_empty() {
            format!("{} ({files_checked} of {files_total} first-party files)", DisplayScope(scope, &cwd))
        } else {
            String::new()
        };
        let notes = RunNotes { command: "deps --check", root: &root_rel, filter: &filter_note, ..RunNotes::default() };
        let summary = Summary { files_checked, filter, notes };
        let mut out = io::stdout().lock();
        let outcome = timing::time("output", || report.write(&opts, &summary, &mut out)).context("cannot write to stdout")?;
        if outcome == Outcome::Written
            && text
            && !llm
            && !scope.is_empty()
        {
            let res = writeln!(
                out,
                "Filtered to {} of {} first-party files; the whole project was analysed.",
                files_checked, files_total
            )
            .and_then(|()| out.flush());
            match res {
                Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e).context("cannot write to stdout"),
                _ => {}
            }
        }
        report.exit_status(opts.min_confidence)
    };

    if opts.stats {
        let s = ix.stats();
        let mut counters = vec![
            ("files", s.files),
            ("first_party_files", u64::from(files_total)),
            ("files_checked", u64::from(files_checked)),
            ("units", u64::from(decls.len())),
            ("declared_dependencies", (0..decls.len()).map(|u| decls.deps(u).len() as u64).sum()),
            ("third_party_imports", u64::from(hs.third_party_rows)),
            ("undeclared_unit_files", u64::from(hs.undeclared_files)),
            ("metadata_reads", u64::from(hs.metadata_reads)),
            ("findings", report.len() as u64),
            ("suppressed", u64::from(filter.suppressed)),
            ("below_confidence", u64::from(filter.below_confidence)),
            ("baselined", u64::from(report.baselined())),
            ("threads", rayon::current_num_threads() as u64),
        ];
        if let Some(cs) = &project.cache {
            counters.extend([("cache_hits", cs.hits + cs.verified), ("cache_misses", cs.misses)]);
        }
        crate::stats::report(t0.elapsed(), &counters);
    }
    Ok(ExitCode::from(status.code()))
}

/// Warns about `pyproject.toml` files that could not be used and about projects whose
/// dependencies cannot be checked (none declared, or dynamic).
fn warn_declarations(decls: &Declarations, root: &std::path::Path, rules: HygieneRules, has_files: bool) {
    for u in 0..decls.len() {
        let d = decls.unit(u);
        if let Some(e) = &d.error {
            warn(format_args!("cannot read {}: {e}; its dependency declarations are ignored", display_rel(&d.path, root)));
        } else if d.status == DeclStatus::Dynamic {
            warn(format_args!(
                "{}: `dependencies` is dynamic; PM601, PM602 and PM604 are not reported for its files",
                display_rel(&d.path, root)
            ));
        }
    }
    let needs_decls = rules.undeclared || rules.unused || rules.transitive;
    if needs_decls && has_files && (0..decls.len()).all(|u| decls.effective(u) == NONE) {
        warn(format_args!(
            "no dependency declarations found (`[project] dependencies` or `[tool.poetry.dependencies]` in \
             pyproject.toml); PM601, PM602 and PM604 need them"
        ));
    }
}

/// Positional paths relative to the working directory, joined by `, ` (the llm filter note).
struct DisplayScope<'a>(&'a [PathBuf], &'a std::path::Path);

impl std::fmt::Display for DisplayScope<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", RelPath(p, self.1))?;
        }
        Ok(())
    }
}
