use clap::{Parser, Subcommand};

use py_mapper::loader::{ThreadCounts, load_modules_with_progress, resolve_thread_counts};
use py_mapper::types::{OutputFormat, Splines};
use py_mapper::{config, cycles, dead_code, grade, graph, metrics, report, tui};

#[derive(Parser)]
#[command(
    name = "pymapper-v1",
    version,
    about = "Analyze Python codebases: call graphs and complexity reports"
)]
struct Cli {
    /// Number of threads for directory walking (0 = auto)
    #[arg(long, global = true, default_value_t = 0)]
    walk_threads: usize,

    /// Number of threads for file parsing (0 = auto)
    #[arg(long, global = true, default_value_t = 0)]
    parse_threads: usize,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a DOT call graph from Python files
    Graph {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Omit external/unresolved nodes and their edges
        #[arg(long)]
        no_externals: bool,

        /// Edge routing style
        #[arg(long, value_enum)]
        splines: Option<Splines>,
    },
    /// Print a complexity report table for all functions
    Report {
        /// Paths to Python files or directories to analyze
        paths: Vec<String>,

        /// Print column descriptions and exit
        #[arg(long)]
        explain: bool,

        /// Only show functions with McC >= this value
        #[arg(long)]
        min_cc: Option<usize>,

        /// Show only the top N functions by McC
        #[arg(long)]
        top: Option<usize>,

        /// Output format
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,

        /// Fail if any function exceeds thresholds (e.g. cc=10,cognitive=15)
        #[arg(long)]
        fail_above: Option<String>,

        /// Show module-level summary instead of per-function report
        #[arg(long)]
        summary: bool,

        /// Show metric distribution histogram
        #[arg(long)]
        histogram: bool,

        /// Sort column (name, cc, cognitive, nesting, loc, params, fi, fo, rfc, depth, tloc, tcc, mi, ret, bool)
        #[arg(long)]
        sort: Option<String>,

        /// Sort in ascending order (default is descending)
        #[arg(long)]
        asc: bool,

        /// Save metrics as JSON baseline to this path
        #[arg(long = "save-baseline")]
        save_baseline: Option<String>,

        /// Compare against a JSON baseline file
        #[arg(long)]
        compare: Option<String>,

        /// Exit 1 if any metric regressed vs baseline
        #[arg(long = "fail-regression", requires = "compare")]
        fail_regression: bool,
    },
    /// Detect functions with zero callers (potential dead code)
    DeadCode {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,

        /// Exclude functions with matching decorator (repeatable)
        #[arg(long = "exclude-decorated")]
        exclude_decorated: Vec<String>,

        /// Exclude methods that override a base class method
        #[arg(long)]
        exclude_overrides: bool,

        /// Show classification column instead of filtering
        #[arg(long)]
        classify: bool,

        /// Show module-level summary instead of per-function listing
        #[arg(long)]
        summary: bool,

        /// Apply a built-in exclusion profile (cli, web, pydantic)
        #[arg(long)]
        profile: Vec<String>,
    },
    /// Detect mutually-recursive function cycles
    Cycles {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
    /// Compute a quality grade/scorecard for the codebase
    Grade {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,

        /// Number of top improvement targets to show
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// Launch the interactive TUI explorer
    Tui {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

fn load_modules(paths: &[String], tc: &ThreadCounts) -> py_mapper::types::ModuleInfo {
    load_modules_with_progress(
        paths,
        ThreadCounts {
            walk: tc.walk,
            parse: tc.parse,
        },
        |_| {},
    )
    .unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    })
}

fn main() {
    let cli = Cli::parse();
    let cfg = config::load_config().unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(2);
    });

    let walk = if cli.walk_threads != 0 {
        cli.walk_threads
    } else {
        cfg.threads.walk.unwrap_or(0)
    };
    let parse = if cli.parse_threads != 0 {
        cli.parse_threads
    } else {
        cfg.threads.parse.unwrap_or(0)
    };
    let tc = resolve_thread_counts(walk, parse);

    match cli.command {
        Command::Graph {
            paths,
            no_externals,
            splines,
        } => {
            let combined = load_modules(&paths, &tc);
            let eff_no_externals = no_externals || cfg.graph.no_externals;
            let eff_splines = splines.unwrap_or_else(|| {
                cfg.graph
                    .splines
                    .as_deref()
                    .and_then(|s| <Splines as clap::ValueEnum>::from_str(s, true).ok())
                    .unwrap_or(Splines::Ortho)
            });
            let call_graph = graph::build_call_graph(&combined, eff_no_externals);
            let dot = graph::emit_dot(&combined, &call_graph, eff_splines);
            print!("{}", dot);
        }
        Command::Report {
            paths,
            explain,
            min_cc,
            top,
            format,
            fail_above,
            summary,
            histogram,
            sort,
            asc,
            save_baseline,
            compare,
            fail_regression,
        } => {
            if explain {
                report::print_column_descriptions();
                return;
            }
            if paths.is_empty() {
                eprintln!("error: paths are required unless --explain is used");
                std::process::exit(1);
            }
            let combined = load_modules(&paths, &tc);
            let call_graph = graph::build_call_graph(&combined, false);
            let gm = metrics::compute_graph_metrics(&call_graph, &combined.functions);

            let thresholds = fail_above.or(cfg.report.fail_above).map(|raw| {
                report::parse_thresholds(&raw).unwrap_or_else(|e| {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                })
            });
            let threshold_failed = thresholds.as_ref().is_some_and(|thresholds| {
                report::print_violations(&report::check_thresholds(&combined, thresholds))
            });

            let eff_min_cc = min_cc.or(cfg.report.min_cc);
            let eff_top = top.or(cfg.report.top);
            let eff_format = format.unwrap_or_else(|| {
                cfg.report
                    .format
                    .as_deref()
                    .and_then(|s| <OutputFormat as clap::ValueEnum>::from_str(s, true).ok())
                    .unwrap_or(OutputFormat::Table)
            });

            if histogram {
                let (rows, _) = report::build_report_rows(&combined, &gm, eff_min_cc, eff_top);
                if rows.is_empty() {
                    println!("No functions found.");
                    std::process::exit(if threshold_failed { 1 } else { 0 });
                }
                let col = sort
                    .as_deref()
                    .or(cfg.report.sort.as_deref())
                    .unwrap_or("cc");
                if report::extract_metric(&rows[0], col).is_none() {
                    eprintln!("warning: unknown sort column: {}", col);
                }
                let eff_col = if report::extract_metric(&rows[0], col).is_some() {
                    col
                } else {
                    "cc"
                };
                let (metric_name, buckets) = report::build_histogram(&rows, eff_col);
                report::print_histogram(&metric_name, &buckets, eff_format);
            } else if summary {
                report::print_module_summary(&combined, &gm, eff_top, eff_format);
            } else {
                let eff_sort = sort.or(cfg.report.sort);
                if let Some(ref col) = eff_sort {
                    let (mut rows, total) =
                        report::build_report_rows(&combined, &gm, eff_min_cc, None);
                    if !report::sort_rows_by_column(&mut rows, col, asc) {
                        eprintln!("error: unknown sort column: {}", col);
                        std::process::exit(2);
                    }
                    if let Some(top) = eff_top {
                        rows.truncate(top);
                    }
                    let filtered = eff_min_cc.is_some() || eff_top.is_some();
                    report::print_sorted_report(&rows, total, filtered, eff_format);
                } else {
                    report::print_report(&combined, &gm, eff_min_cc, eff_top, eff_format);
                }
            }

            if save_baseline.is_some() || compare.is_some() {
                let (rows, _) = report::build_report_rows(&combined, &gm, None, None);
                if let Some(ref path) = save_baseline {
                    report::save_baseline(&rows, path).unwrap_or_else(|e| {
                        eprintln!("error: {}", e);
                        std::process::exit(1);
                    });
                }
                if let Some(ref path) = compare {
                    let baseline = report::load_baseline(path).unwrap_or_else(|e| {
                        eprintln!("error: {}", e);
                        std::process::exit(1);
                    });
                    if report::compare_baseline(&rows, &baseline, fail_regression) {
                        std::process::exit(1);
                    }
                }
            }

            if threshold_failed {
                std::process::exit(1);
            }
        }
        Command::DeadCode {
            paths,
            format,
            exclude_decorated,
            exclude_overrides,
            classify,
            summary,
            profile,
        } => {
            let combined = load_modules(&paths, &tc);
            let call_graph = graph::build_call_graph(&combined, false);
            let gm = metrics::compute_graph_metrics(&call_graph, &combined.functions);

            let mut eff_exclude = exclude_decorated;
            eff_exclude.extend(cfg.dead_code.exclude_decorated.iter().cloned());
            let eff_overrides = exclude_overrides || cfg.dead_code.exclude_overrides;
            let mut eff_profile = profile;
            eff_profile.extend(cfg.dead_code.profile.iter().cloned());

            let has_options =
                classify || eff_overrides || !eff_exclude.is_empty() || !eff_profile.is_empty();
            if has_options {
                let options = dead_code::DeadCodeOptions {
                    exclude_decorated: eff_exclude,
                    exclude_overrides: eff_overrides,
                    classify,
                    profile: eff_profile,
                };
                let entries = dead_code::find_dead_code_classified(
                    &combined.functions,
                    &combined.classes,
                    &gm,
                    &options,
                );
                if summary {
                    let rows = dead_code::build_dead_code_summary_classified(&entries);
                    dead_code::print_dead_code_summary(&rows, format);
                } else {
                    dead_code::print_dead_code_classified(&entries, format);
                }
            } else if summary {
                let dead = dead_code::find_dead_code(&combined.functions, &combined.classes, &gm);
                let rows = dead_code::build_dead_code_summary(&dead);
                dead_code::print_dead_code_summary(&rows, format);
            } else {
                let dead = dead_code::find_dead_code(&combined.functions, &combined.classes, &gm);
                dead_code::print_dead_code(&dead, format);
            }
        }
        Command::Cycles { paths, format } => {
            let combined = load_modules(&paths, &tc);
            let call_graph = graph::build_call_graph(&combined, true);
            let found = cycles::find_cycles(&call_graph);
            cycles::print_cycles(&found, format);
        }
        Command::Grade { paths, format, top } => {
            let combined = load_modules(&paths, &tc);
            let call_graph = graph::build_call_graph(&combined, false);
            let gm = metrics::compute_graph_metrics(&call_graph, &combined.functions);
            grade::print_grade(&combined, &gm, format, top);
        }
        Command::Tui { paths } => {
            if let Err(e) = tui::run_tui(&paths, tc) {
                eprintln!("TUI error: {}", e);
                std::process::exit(1);
            }
        }
    }
}
