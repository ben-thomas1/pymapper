//! `pymapper config` subcommand, and [`ConfigArgs`]: the configuration flags shared by every
//! subcommand that loads the config (flatten it into the subcommand's `Args`).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use pymapper_core::config::{self, Loaded, Mode, OutputFormat, Overrides, Preset, RuleSelector};

/// Configuration flags. They override the discovered config file; see `pymapper config
/// --template` for every option.
#[derive(clap::Args, Debug, Default)]
pub struct ConfigArgs {
    /// Use this config file (pymapper.toml syntax, or a pyproject.toml with [tool.pymapper])
    /// instead of discovering one.
    #[arg(long, value_name = "FILE", help_heading = "Configuration")]
    pub config: Option<PathBuf>,
    /// Ignore all config files.
    #[arg(long, conflicts_with = "config", help_heading = "Configuration")]
    pub isolated: bool,
    /// Override an option with a TOML fragment in pymapper.toml syntax, e.g.
    /// --set 'thresholds.max-mccabe = 12' or --set 'mode = "library"'. Repeatable.
    #[arg(long = "set", value_name = "TOML", help_heading = "Configuration")]
    pub set: Vec<String>,
    /// Rules to enable, replacing `rules.select` (comma-separated selectors: ALL, PM1, PM101).
    #[arg(long, value_delimiter = ',', value_name = "SELECTORS", help_heading = "Configuration")]
    pub select: Option<Vec<RuleSelector>>,
    /// Rules to enable in addition to the selected ones.
    #[arg(long, value_delimiter = ',', value_name = "SELECTORS", help_heading = "Configuration")]
    pub extend_select: Vec<RuleSelector>,
    /// Rules to disable (added to `rules.ignore`).
    #[arg(long, value_delimiter = ',', value_name = "SELECTORS", help_heading = "Configuration")]
    pub ignore: Vec<RuleSelector>,
    /// Extra gitignore-style exclude glob, relative to the project root. Repeatable.
    #[arg(long, value_name = "GLOB", help_heading = "Configuration")]
    pub exclude: Vec<String>,
    /// Enable framework presets (flask, fastapi, django, click, pytest, pydantic).
    #[arg(long, value_delimiter = ',', value_name = "PRESETS", help_heading = "Configuration")]
    pub preset: Vec<Preset>,
    /// Output format: concise, json, sarif, github.
    #[arg(long, visible_alias = "format", value_name = "FORMAT", help_heading = "Configuration")]
    pub output_format: Option<OutputFormat>,
    /// Minimum finding confidence (0-100) to report.
    #[arg(long, visible_alias = "confidence", value_name = "N", value_parser = clap::value_parser!(u8).range(0..=100), help_heading = "Configuration")]
    pub min_confidence: Option<u8>,
    /// Cache directory.
    #[arg(long, value_name = "DIR", help_heading = "Configuration")]
    pub cache_dir: Option<PathBuf>,
    /// Do not read or write the cache.
    #[arg(long, help_heading = "Configuration")]
    pub no_cache: bool,
    /// Analyse third-party packages from the virtualenv.
    #[arg(long, overrides_with = "no_deps", help_heading = "Configuration")]
    pub deps: bool,
    /// Do not analyse third-party packages.
    #[arg(long, overrides_with = "deps", help_heading = "Configuration")]
    pub no_deps: bool,
    /// Analyse the standard library.
    #[arg(long, overrides_with = "no_stdlib", help_heading = "Configuration")]
    pub stdlib: bool,
    /// Do not analyse the standard library.
    #[arg(long, overrides_with = "stdlib", help_heading = "Configuration")]
    pub no_stdlib: bool,
    /// Virtualenv to use.
    #[arg(long, value_name = "DIR", help_heading = "Configuration")]
    pub venv: Option<PathBuf>,
    /// Dead-code mode: auto, application, library.
    #[arg(long, help_heading = "Configuration")]
    pub mode: Option<Mode>,
    /// Worker threads (0 = one per logical CPU).
    #[arg(long, short = 'j', value_name = "N", value_parser = clap::value_parser!(u32).range(0..=config::MAX_THREADS as i64), help_heading = "Configuration")]
    pub threads: Option<u32>,
    /// coverage.py JSON report (`coverage json`) used as dead-code evidence (overrides
    /// `analysis.coverage`). Only `check` reads it.
    #[arg(long, value_name = "FILE", overrides_with = "no_coverage", help_heading = "Configuration")]
    pub coverage: Option<PathBuf>,
    /// Ignore the configured coverage report.
    #[arg(long, overrides_with = "coverage", help_heading = "Configuration")]
    pub no_coverage: bool,
}

/// `--flag` / `--no-flag` pair to an optional override.
fn tristate(yes: bool, no: bool) -> Option<bool> {
    if yes {
        Some(true)
    } else if no {
        Some(false)
    } else {
        None
    }
}

impl ConfigArgs {
    /// Builds the core overrides, moving the parsed values. `paths` are the subcommand's
    /// positional paths.
    pub fn into_overrides(self, paths: Vec<PathBuf>) -> Overrides {
        Overrides {
            config_file: self.config,
            isolated: self.isolated,
            paths,
            set: self.set,
            select: self.select,
            extend_select: self.extend_select,
            ignore: self.ignore,
            extend_exclude: self.exclude,
            presets: self.preset,
            output_format: self.output_format,
            min_confidence: self.min_confidence,
            cache_dir: self.cache_dir,
            cache: self.no_cache.then_some(false),
            include_deps: tristate(self.deps, self.no_deps),
            include_stdlib: tristate(self.stdlib, self.no_stdlib),
            venv: self.venv,
            mode: self.mode,
            threads: self.threads,
            coverage: self.coverage,
            no_coverage: self.no_coverage,
        }
    }

    /// Discovers, applies and validates the configuration; prints warnings to stderr.
    pub fn load(self, paths: Vec<PathBuf>) -> anyhow::Result<Loaded> {
        let cwd = std::env::current_dir().context("cannot determine the working directory")?;
        let loaded = config::load(&cwd, &self.into_overrides(paths))?;
        let mut err = io::stderr().lock();
        for w in &loaded.warnings {
            // Ignore write failures: stderr may be closed and there is nowhere else to report.
            let _ = writeln!(err, "pymapper: warning: {w}");
        }
        Ok(loaded)
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Print the fully commented configuration template (every option, its default and cost).
    #[arg(long, conflicts_with_all = ["paths", "config", "isolated", "set"])]
    template: bool,
    /// Paths to report on (as for `check`); config and project-root discovery start at the
    /// first (default: working directory).
    paths: Vec<PathBuf>,
    #[command(flatten)]
    config: ConfigArgs,
}

/// Prints the effective configuration (pymapper.toml syntax) or the template.
pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let text = if args.template {
        config::TEMPLATE.to_owned()
    } else {
        let loaded = args.config.load(args.paths)?;
        let source = match &loaded.project.config_file {
            Some((path, _)) => path.display().to_string(),
            None => "none (defaults)".to_owned(),
        };
        let mut text = format!(
            "# Effective pymapper configuration\n# project root: {}\n# config file: {source}\n\n",
            loaded.project.root.display()
        );
        text.push_str(&loaded.config.to_toml());
        text
    };
    let mut out = io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        // A closed pipe (`pymapper config | head`) is not an error.
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e).context("cannot write to stdout"),
        _ => Ok(ExitCode::SUCCESS),
    }
}
