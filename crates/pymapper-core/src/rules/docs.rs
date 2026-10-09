//! Generates `docs/rules.md` from the registry. The checked-in file is compared against this
//! output by a test; regenerate it with
//! `PYMAPPER_BLESS=1 cargo test -p pymapper-core rules::tests::docs_up_to_date`.

use std::fmt::Write;

use crate::config::Thresholds;

use super::finding::{Piece, pieces};
use super::{ArgKind, Bound, Category, ConfidenceModel, Gate, RuleDef};

/// Renders the rule reference as Markdown.
pub fn render_markdown() -> String {
    let mut out = String::with_capacity(32 * 1024);
    // Writing to a `String` cannot fail.
    let _ = render(&mut out);
    out
}

fn render(out: &mut String) -> std::fmt::Result {
    let defaults = Thresholds::default();
    out.push_str(HEADER);
    writeln!(out, "| range | category | rules |")?;
    writeln!(out, "|-------|----------|-------|")?;
    for &cat in Category::ALL {
        let rules = super::in_category(cat);
        writeln!(out, "| PM{}xx | [{}](#{}) | {} |", cat.digit(), cat.title(), anchor(cat.title()), rules.len())?;
    }
    out.push_str("\nCodes PM000-PM099 and PM700-PM999 are unassigned.\n\n## All rules\n\n");
    writeln!(out, "| code | name | severity | default | cost | confidence | summary |")?;
    writeln!(out, "|------|------|----------|---------|------|------------|---------|")?;
    for def in super::all() {
        writeln!(
            out,
            "| [PM{:03}](#pm{:03}-{}) | `{}` | {} | {} | {} | {} | {} |",
            def.code,
            def.code,
            def.name,
            def.name,
            def.severity.as_str(),
            default_cell(def),
            def.cost.as_str(),
            match def.confidence {
                ConfidenceModel::Certain => "100",
                ConfidenceModel::Scored(_) => "scored",
            },
            def.summary,
        )?;
    }
    for &cat in Category::ALL {
        writeln!(out, "\n## {}\n", cat.title())?;
        for def in super::in_category(cat) {
            rule_section(out, def, &defaults)?;
        }
    }
    Ok(())
}

fn default_cell(def: &RuleDef) -> String {
    match (def.default_enabled, def.gate.key()) {
        (false, _) => "off".to_owned(),
        (true, None) => "on".to_owned(),
        (true, Some(key)) => format!("needs `{key}`"),
    }
}

fn rule_section(out: &mut String, def: &RuleDef, defaults: &Thresholds) -> std::fmt::Result {
    writeln!(out, "### PM{:03} {}\n", def.code, def.name)?;
    writeln!(out, "{}.\n", def.summary)?;
    writeln!(out, "- Message: `{}`", message_shape(def))?;
    writeln!(out, "- Severity: {}", def.severity.as_str())?;
    let enabled = match (def.default_enabled, def.gate) {
        (false, _) => "no (enable with `select` / `extend-select`)".to_owned(),
        (true, Gate::None) => "yes".to_owned(),
        (true, gate) => format!("yes, when `{} = true`", gate.key().unwrap_or_default()),
    };
    writeln!(out, "- Enabled by default: {enabled}")?;
    writeln!(out, "- Cost: {}", def.cost.as_str())?;
    for &key in def.thresholds {
        let what = match key.bound() {
            Bound::Max => "reported above",
            Bound::Min => "reported below",
            Bound::Param => "parameter",
        };
        writeln!(out, "- Threshold: `thresholds.{}` ({what}; default {})", key.key(), key.get(defaults))?;
    }
    match def.confidence {
        ConfidenceModel::Certain => writeln!(out, "- Confidence: always 100")?,
        ConfidenceModel::Scored(how) => writeln!(out, "- Confidence: {how}")?,
    }
    writeln!(out, "\n{}\n", def.explanation)?;
    writeln!(out, "Suppress on a line with `# pymapper: ignore[PM{:03}]`.\n", def.code)?;
    Ok(())
}

/// The message template with placeholders shown as `<kind>` (backticks dropped: the result is
/// shown in a code span).
fn message_shape(def: &RuleDef) -> String {
    let mut out = String::new();
    for piece in pieces(def.message) {
        match piece {
            Ok(Piece::Lit(s)) => out.push_str(&s.replace('`', "")),
            Ok(Piece::Arg(i)) => match def.args.get(i) {
                // The wordings, `[a | b]`.
                Some(ArgKind::Phrase(words)) => {
                    let _ = write!(out, "[{}]", words.join(" | "));
                }
                kind => out.push_str(kind_name(kind)),
            },
            Ok(Piece::Rest(i, sep)) => {
                let k = kind_name(def.args.get(i));
                let _ = write!(out, "{k}{sep}{k}{sep}...");
            }
            Err(_) => break,
        }
    }
    out
}

fn kind_name(kind: Option<&ArgKind>) -> &'static str {
    match kind {
        Some(ArgKind::U32) => "<int>",
        Some(ArgKind::F32) => "<real>",
        Some(ArgKind::Symbol) => "<symbol>",
        Some(ArgKind::Name) => "<name>",
        Some(ArgKind::File) => "<file>",
        Some(ArgKind::Contract) => "<contract>",
        Some(ArgKind::Phrase(_)) => "<phrase>",
        None => "?",
    }
}

/// GitHub-style heading anchor: lower case, spaces to `-`, other punctuation dropped.
fn anchor(title: &str) -> String {
    title
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

const HEADER: &str = "<!-- Generated from crates/pymapper-core/src/rules by rules::render_markdown. Do not edit; \
regenerate with `PYMAPPER_BLESS=1 cargo test -p pymapper-core rules::tests::docs_up_to_date`. -->

# pymapper rules

Every finding belongs to a rule with a code `PMxxx`. Codes are grouped by category in ranges of
one hundred.

## Selecting rules

Rules are chosen in `[rules]` (or with `--select`, `--extend-select`, `--ignore`) by code
prefix: `ALL`, `PM`, `PM1`, `PM10`, `PM101`. Without `select`, the rules marked \"on\" below
run. When a rule matches both a select and an ignore selector, the more specific selector wins;
on a tie, ignore wins. `per-file-ignores` disables rules in files matching a glob.

Expensive rules also need their `[analysis]` toggle (\"needs ...\" below): turning the toggle on
enables them unless they are ignored, and selecting them without the toggle runs nothing (pymapper
prints a note). Analyses whose rules are all disabled do not run at all.

## Thresholds

Metric rules read a limit from `[thresholds]` and report values strictly above a `max-*` limit
(strictly below a `min-*` limit).

## Confidence

Every finding has a confidence from 0 to 100. Measured facts (metrics, contract violations)
always have 100; heuristic rules (dead code, dependency mapping) score each finding as described
per rule. `pymapper check` reports findings with confidence at or above `min-confidence`
(default 80, `--confidence`) and exits 1 if there are any.

## Suppressing findings

Add `# pymapper: ignore[CODES]` to the line a finding is reported on (the `def` / `class` line,
the import line, or line 1 for module-level findings). Codes are comma-separated selectors, so
`# pymapper: ignore[PM101, PM4]` suppresses PM101 and every dead-code rule on that line; a bare
`# pymapper: ignore` suppresses every rule. Malformed codes (`E501`, `pm101`) are reported as
diagnostics and ignored.

Ruff's `# noqa` is honoured for unused imports only: a bare `# noqa` or a `# noqa` listing F401
(`# noqa: F401`, `# noqa:F401,E501`, any case) suppresses PM405 on its line, or on every name of
a multi-line import when it is on the statement's first line. Other codes suppress nothing.

## Categories

";
