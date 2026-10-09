//! Which selected rules `check` can run, and the messages about the others.
//!
//! ```text
//!   selected rule ──> gate closed (analysis.dupes)     ──> skipped: Gate
//!                 ──> PM6xx (deps --check)             ──> skipped: Deps
//!                 ──> PM302-PM304 without [[contracts]] ──> skipped: NoContracts
//!                 ──> otherwise                         ──> runs
//! ```
//!
//! Nothing runnable is a usage error (exit 2), never "All checks passed!". Skipped rules that
//! were selected by name or category are warned about; whenever the user selected rules
//! (`select` / `extend-select`, `ALL` included) the report footer says how many ran and which
//! were skipped.

use std::fmt;
use std::path::{Path, PathBuf};

use pymapper_core::config::{Config, RuleSelector};
use pymapper_core::rules::{self, Pass, RuleEngine};

use crate::pipeline::warn;

/// Why a selected rule does not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Expensive, and its `[analysis]` toggle (the key) is off.
    Gate(&'static str),
    /// A dependency-hygiene rule: `pymapper deps --check` runs them.
    Deps,
    /// A layering-contract rule without any `[[contracts]]`.
    NoContracts,
}

impl Skip {
    /// Why, and how to enable.
    pub fn why(self) -> SkipWhy {
        SkipWhy(self)
    }
}

/// Short form of a [`Skip`] reason, for the footer.
pub struct SkipShort(Skip);

impl fmt::Display for SkipShort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Skip::Gate(key) => write!(f, "needs `{key} = true`"),
            Skip::Deps => f.write_str("run by `pymapper deps --check`"),
            Skip::NoContracts => f.write_str("no [[contracts]]"),
        }
    }
}

/// Display of [`Skip::why`].
pub struct SkipWhy(Skip);

impl fmt::Display for SkipWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Skip::Gate(key) => write!(f, "expensive, needs `{key} = true` (e.g. --set '{key} = true')"),
            Skip::Deps => f.write_str("dependency rules are run by `pymapper deps --check`, not `check`"),
            Skip::NoContracts => f.write_str("no [[contracts]] are configured"),
        }
    }
}

/// Skipped rules sharing a reason, in code order.
pub struct Group {
    pub reason: Skip,
    pub codes: Vec<u16>,
}

/// The selected rules split into those that run and those that are skipped.
pub struct RulePlan {
    /// Selected rules (after `ignore`).
    pub selected: u32,
    /// Selected rules that run.
    pub ran: u32,
    /// Skipped rules: code, reason, and the specificity of the most specific `select` /
    /// `extend-select` selector naming it (`None`: selected by default).
    pub skipped: Vec<(u16, Skip, Option<u8>)>,
    /// The user selected rules (`select` set, or a non-empty `extend-select`).
    pub explicit: bool,
}

impl RulePlan {
    pub fn new(config: &Config, engine: &RuleEngine) -> RulePlan {
        let r = &config.rules;
        let user: Vec<RuleSelector> = r.select.iter().flatten().chain(&r.extend_select).copied().collect();
        let mut plan = RulePlan { selected: 0, ran: 0, skipped: Vec::new(), explicit: r.select.is_some() || !r.extend_select.is_empty() };
        for def in rules::all() {
            if !engine.selected().contains(def.rule()) {
                continue;
            }
            plan.selected += 1;
            let skip = if !engine.is_enabled(def.rule()) {
                Some(Skip::Gate(def.gate.key().unwrap_or("?")))
            } else if def.pass == Pass::Deps {
                Some(Skip::Deps)
            } else if def.pass == Pass::Layers && def.code != rules::codes::IMPORT_CYCLE.0 && config.contracts.is_empty() {
                Some(Skip::NoContracts)
            } else {
                None
            };
            match skip {
                Some(why) => {
                    let named = user.iter().filter(|s| s.matches(def.code)).map(|s| s.specificity()).max();
                    plan.skipped.push((def.code, why, named));
                }
                None => plan.ran += 1,
            }
        }
        plan
    }

    /// Fails when nothing would run: no rule selected, or none of the selected can run here.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.selected == 0 {
            anyhow::bail!("no rule is selected: `select` / `--select` and `ignore` / `--ignore` leave nothing to check");
        }
        if self.ran == 0 {
            let mut msg = String::from("none of the selected rules can run in `check`:");
            for g in self.groups() {
                msg.push_str(&format!("\n  {}: {}", CodeList(&g.codes), g.reason.why()));
            }
            anyhow::bail!(msg);
        }
        Ok(())
    }

    /// Skipped rules grouped by reason (gates first, then dependency rules, then contracts).
    pub fn groups(&self) -> Vec<Group> {
        let mut out: Vec<Group> = Vec::new();
        for &(code, reason, _) in &self.skipped {
            match out.iter_mut().find(|g| g.reason == reason) {
                Some(g) => g.codes.push(code),
                None => out.push(Group { reason, codes: vec![code] }),
            }
        }
        out
    }

    /// Warns about skipped rules the user named by code or category (`PM501`, `PM6`); `ALL`
    /// and `PM` stay quiet (the footer lists them). Contract rules only warn when named by
    /// exact code: `PM3` without contracts is a normal way to ask for cycles.
    pub fn warn_skipped(&self) {
        let mut named: Vec<Group> = Vec::new();
        for &(code, reason, spec) in &self.skipped {
            let min = if reason == Skip::NoContracts { 4 } else { 2 };
            if spec.is_none_or(|s| s < min) {
                continue;
            }
            match named.iter_mut().find(|g| g.reason == reason) {
                Some(g) => g.codes.push(code),
                None => named.push(Group { reason, codes: vec![code] }),
            }
        }
        for g in &named {
            let verb = if g.codes.len() == 1 { "is selected but does not run" } else { "are selected but do not run" };
            warn(format_args!("{} {verb}: {}", CodeList(&g.codes), g.reason.why()));
        }
    }
}

/// Codes as `PM601-PM603, PM605`: runs of consecutive codes collapsed.
pub struct CodeList<'a>(pub &'a [u16]);

impl fmt::Display for CodeList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.0;
        let mut i = 0;
        while i < c.len() {
            let mut j = i;
            while j + 1 < c.len() && c[j + 1] == c[j] + 1 {
                j += 1;
            }
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "PM{:03}", c[i])?;
            if j > i {
                write!(f, "-PM{:03}", c[j])?;
            }
            i = j + 1;
        }
        Ok(())
    }
}

/// Fails on selectors that match no registered rule (`--select PM999`), anywhere in the rule
/// options: they would silently select or ignore nothing.
pub fn check_selectors(config: &Config) -> anyhow::Result<()> {
    let r = &config.rules;
    let pfi = r.per_file_ignores.iter().flat_map(|e| &e.rules);
    for &sel in r.select.iter().flatten().chain(&r.extend_select).chain(&r.ignore).chain(pfi) {
        if rules::selector_is_known(sel) {
            continue;
        }
        let help = match rules::suggest_selector(sel) {
            Some(s) => format!("did you mean `{s}`? "),
            None => String::new(),
        };
        anyhow::bail!("unknown rule selector `{sel}`: it matches no rule\n  help: {help}`pymapper config --template` and docs/rules.md list every rule");
    }
    Ok(())
}

/// Extra footer lines after the report (concise / github formats): the path filter and, when
/// the user selected rules, how many ran and which were skipped.
pub struct Footnote<'a> {
    pub scope: &'a [PathBuf],
    pub root: &'a Path,
    pub files_checked: u32,
    pub files_total: u32,
    pub plan: &'a RulePlan,
}

impl fmt::Display for Footnote<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.scope.is_empty() {
            f.write_str("Filtered to ")?;
            for (i, p) in self.scope.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", super::display_rel(p, self.root))?;
            }
            writeln!(
                f,
                " ({} of {} first-party files); the whole project was analysed.",
                self.files_checked, self.files_total
            )?;
        }
        let plan = self.plan;
        if plan.explicit && !plan.skipped.is_empty() {
            let rule = |n: u32| if n == 1 { "rule" } else { "rules" };
            write!(f, "{} {} ran, {} skipped (", plan.ran, rule(plan.ran), plan.skipped.len())?;
            for (i, g) in plan.groups().iter().enumerate() {
                if i > 0 {
                    f.write_str("; ")?;
                }
                write!(f, "{}: {}", CodeList(&g.codes), SkipShort(g.reason))?;
            }
            f.write_str(").\n")?;
        }
        Ok(())
    }
}
