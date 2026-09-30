use clap::{Parser, Subcommand};

use py_mapper::v2;

#[derive(Parser)]
#[command(
    name = "pymapper",
    version,
    about = "Analyze Python codebases with a confidence-aware semantic graph"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Find improvement targets: hotspots, dead-code candidates, unresolved calls, and coupling risks
    Scan {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format: table, json, dot
        #[arg(long, default_value = "table")]
        format: String,

        /// Analysis profile: general, pytest
        #[arg(long, default_value = "general")]
        profile: String,

        /// Include edges/findings up to this confidence: certain, likely, possible, unresolved
        #[arg(long, default_value = "unresolved")]
        confidence: String,
    },
    /// Emit the semantic graph and analysis bundle
    Map {
        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format: table, json, dot
        #[arg(long, default_value = "table")]
        format: String,

        /// Analysis profile: general, pytest
        #[arg(long, default_value = "general")]
        profile: String,

        /// Include edges up to this confidence: certain, likely, possible, unresolved
        #[arg(long, default_value = "possible")]
        confidence: String,
    },
    /// Explain a module, symbol, or path match
    Explain {
        /// Symbol name, qualified-name fragment, symbol ID, or file suffix
        query: String,

        /// Paths to Python files or directories to analyze
        #[arg(required = true)]
        paths: Vec<String>,

        /// Output format: table, json
        #[arg(long, default_value = "table")]
        format: String,

        /// Analysis profile: general, pytest
        #[arg(long, default_value = "general")]
        profile: String,

        /// Include edges up to this confidence: certain, likely, possible, unresolved
        #[arg(long, default_value = "possible")]
        confidence: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan {
            paths,
            format,
            profile,
            confidence,
        } => {
            let format = parse_format(&format, true);
            let profile = parse_profile(&profile);
            let confidence = parse_confidence(&confidence);
            let bundle = analyze(&paths, profile);
            v2::print_scan(&bundle, format, confidence);
        }
        Command::Map {
            paths,
            format,
            profile,
            confidence,
        } => {
            let format = parse_format(&format, true);
            let profile = parse_profile(&profile);
            let confidence = parse_confidence(&confidence);
            let bundle = analyze(&paths, profile);
            v2::print_map(&bundle, format, confidence);
        }
        Command::Explain {
            query,
            paths,
            format,
            profile,
            confidence,
        } => {
            let format = parse_format(&format, false);
            let profile = parse_profile(&profile);
            let confidence = parse_confidence(&confidence);
            let bundle = analyze(&paths, profile);
            if let Err(e) = v2::print_explain(&bundle, &query, format, confidence) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn analyze(paths: &[String], profile: v2::Profile) -> v2::AnalysisBundle {
    v2::analyze_paths(paths, profile).unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    })
}

fn parse_format(raw: &str, allow_dot: bool) -> v2::OutputFormat {
    let format = v2::parse_format(raw).unwrap_or_else(|| {
        eprintln!("error: unknown format '{}'", raw);
        std::process::exit(2);
    });
    if !allow_dot && format == v2::OutputFormat::Dot {
        eprintln!("error: explain supports only table and json formats");
        std::process::exit(2);
    }
    format
}

fn parse_profile(raw: &str) -> v2::Profile {
    v2::parse_profile(raw).unwrap_or_else(|| {
        eprintln!("error: unknown profile '{}'", raw);
        std::process::exit(2);
    })
}

fn parse_confidence(raw: &str) -> v2::Confidence {
    v2::parse_confidence(raw).unwrap_or_else(|| {
        eprintln!("error: unknown confidence '{}'", raw);
        std::process::exit(2);
    })
}
