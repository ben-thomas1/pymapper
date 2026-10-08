//! Rule selection, gating, thresholds and finding filtering for one run.

use std::collections::HashMap;

use crate::config::{AnalysisConfig, Config, RuleConfig, Thresholds};

use super::{FileId, Finding, Findings, Pass, Rule, RuleDef, RuleSet, Suppressions, ThresholdKey};

/// Per-run rule state, resolved once from the config. Cheap to share across threads (`Sync`,
/// no interior mutability); every query is a bit test.
#[derive(Clone, Debug)]
pub struct RuleEngine {
    /// Selected by `select` / `extend-select` / `ignore` (or the defaults).
    selected: RuleSet,
    /// Selected and with an open gate: the rules that run.
    enabled: RuleSet,
    /// Bit per [`Pass`] with at least one enabled rule.
    passes: u32,
    thresholds: Thresholds,
    min_confidence: u8,
    /// Rules ignored by each `per-file-ignores` entry (parallel to the config list).
    per_file: Vec<RuleSet>,
    /// Union of `per_file`: findings of other rules skip path matching entirely.
    per_file_any: RuleSet,
}

/// What [`RuleEngine::filter`] does with a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Reported.
    Report,
    /// Its rule is not enabled (analyses should not emit these; dropped defensively).
    Disabled,
    /// Ignored by a `per-file-ignores` entry matching its file.
    FileIgnored,
    /// Suppressed by an inline `# pymapper: ignore[...]` on its line.
    Suppressed,
    /// Confidence below `min-confidence`.
    BelowConfidence,
}

/// Counts per [`Verdict`] from one [`RuleEngine::filter`] call (for the summary footer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterStats {
    /// Findings at or above the confidence gate, kept.
    pub reported: u32,
    /// Findings below the confidence gate (kept or dropped, see `filter`).
    pub below_confidence: u32,
    /// Dropped: rule disabled.
    pub disabled: u32,
    /// Dropped: `per-file-ignores`.
    pub file_ignored: u32,
    /// Dropped: inline suppression.
    pub suppressed: u32,
}

/// Sentinel for "file mask not computed yet" in the filter's per-file cache.
const UNSET: u32 = u32::MAX;

impl RuleEngine {
    /// Resolves the rule options of `config`.
    pub fn new(config: &Config) -> RuleEngine {
        RuleEngine::from_parts(&config.rules, &config.analysis, &config.thresholds, config.min_confidence)
    }

    /// Resolves rule state from the individual option groups. `min_confidence` above 100 is
    /// treated as 100.
    pub fn from_parts(
        rules: &RuleConfig,
        analysis: &AnalysisConfig,
        thresholds: &Thresholds,
        min_confidence: u8,
    ) -> RuleEngine {
        let mut selected = RuleSet::EMPTY;
        let mut enabled = RuleSet::EMPTY;
        let mut passes = 0;
        for def in super::all() {
            if !rules.is_enabled(def.code, def.default_enabled) {
                continue;
            }
            selected.insert(def.rule());
            if def.gate.is_open(analysis) {
                enabled.insert(def.rule());
                passes |= def.pass.bit();
            }
        }
        let per_file: Vec<RuleSet> = rules
            .per_file_ignores
            .iter()
            .map(|pfi| super::all().filter(|d| pfi.ignores(d.code)).map(RuleDef::rule).collect::<RuleSet>().intersection(&enabled))
            .collect();
        let per_file_any = per_file.iter().fold(RuleSet::EMPTY, |acc, s| acc.union(s));
        RuleEngine {
            selected,
            enabled,
            passes,
            thresholds: thresholds.clone(),
            min_confidence: min_confidence.min(100),
            per_file,
            per_file_any,
        }
    }

    /// Whether the rule runs: selected and, if expensive, its gate is open.
    #[inline]
    pub fn is_enabled(&self, rule: Rule) -> bool {
        self.enabled.contains(rule)
    }

    /// The rules that run.
    #[inline]
    pub fn enabled(&self) -> &RuleSet {
        &self.enabled
    }

    /// The selected rules, including expensive ones whose gate is closed.
    #[inline]
    pub fn selected(&self) -> &RuleSet {
        &self.selected
    }

    /// Selected rules that do not run because their gate is closed, for a "PM501 needs
    /// `analysis.dupes = true`" note.
    pub fn gated_off(&self) -> impl Iterator<Item = &'static RuleDef> + '_ {
        self.selected.difference(&self.enabled).iter().map(Rule::def)
    }

    /// Whether a pass has any enabled rule. Analyses check this before doing any work.
    #[inline]
    pub fn should_run(&self, pass: Pass) -> bool {
        self.passes & pass.bit() != 0
    }

    /// The configured thresholds.
    #[inline]
    pub fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }

    /// Whether `value` violates the threshold (see [`ThresholdKey::violated_by`]).
    #[inline]
    pub fn violates(&self, key: ThresholdKey, value: f64) -> bool {
        key.violated_by(&self.thresholds, value)
    }

    /// Minimum confidence (0-100) for a finding to be reported.
    #[inline]
    pub fn min_confidence(&self) -> u8 {
        self.min_confidence
    }

    /// Rules ignored in a file, given which `per-file-ignores` entries match its path
    /// (`matches(i)` for entry `i` of `Config::rules.per_file_ignores`). Only entries that
    /// ignore an enabled rule are asked.
    pub fn file_ignores(&self, mut matches: impl FnMut(usize) -> bool) -> RuleSet {
        let mut out = RuleSet::EMPTY;
        for (i, set) in self.per_file.iter().enumerate() {
            if !set.is_empty() && matches(i) {
                out = out.union(set);
            }
        }
        out
    }

    /// The verdict for one finding, given the rules ignored in its file.
    #[inline]
    pub fn verdict(&self, f: &Finding, file_ignored: &RuleSet, suppressions: &Suppressions) -> Verdict {
        if !self.enabled.contains(f.rule) {
            Verdict::Disabled
        } else if file_ignored.contains(f.rule) {
            Verdict::FileIgnored
        } else if f.loc.file != FileId::NONE && suppressions.is_suppressed(f.loc.file, f.loc.line, f.rule) {
            Verdict::Suppressed
        } else if f.confidence < self.min_confidence {
            Verdict::BelowConfidence
        } else {
            Verdict::Report
        }
    }

    /// Applies rule selection, `per-file-ignores`, inline suppressions and the confidence gate
    /// in place, preserving order. Findings below `min-confidence` are kept when
    /// `keep_below_confidence` (for JSON output or the GUI's confidence filter), else dropped.
    ///
    /// `path_matches(file, i)` answers whether entry `i` of `Config::rules.per_file_ignores`
    /// matches `file`'s path (the walker's gitignore matcher owns the glob syntax). It is called
    /// at most once per (file with a finding of a per-file-ignored rule, relevant entry), never
    /// per finding. Run this before sorting for output: it is allocation-free on unsorted
    /// buffers except for a 4-byte-per-file cache when `per-file-ignores` is in use.
    pub fn filter(
        &self,
        findings: &mut Findings,
        suppressions: &Suppressions,
        keep_below_confidence: bool,
        mut path_matches: impl FnMut(FileId, usize) -> bool,
    ) -> FilterStats {
        let mut stats = FilterStats::default();
        // file id -> index into `masks`, filled lazily.
        let mut mask_of: Vec<u32> = Vec::new();
        let mut masks: Vec<RuleSet> = Vec::new();
        let mut mask_ids: HashMap<RuleSet, u32> = HashMap::new();
        findings.retain(|f, _| {
            let file = f.loc.file;
            let ignored = if file != FileId::NONE && self.per_file_any.contains(f.rule) {
                let idx = file.0 as usize;
                if idx >= mask_of.len() {
                    mask_of.resize(idx + 1, UNSET);
                }
                if mask_of[idx] == UNSET {
                    let mask = self.file_ignores(|i| path_matches(file, i));
                    mask_of[idx] = *mask_ids.entry(mask).or_insert_with(|| {
                        masks.push(mask);
                        (masks.len() - 1) as u32
                    });
                }
                masks[mask_of[idx] as usize]
            } else {
                RuleSet::EMPTY
            };
            match self.verdict(f, &ignored, suppressions) {
                Verdict::Report => {
                    stats.reported += 1;
                    true
                }
                Verdict::BelowConfidence => {
                    stats.below_confidence += 1;
                    keep_below_confidence
                }
                Verdict::Disabled => {
                    stats.disabled += 1;
                    false
                }
                Verdict::FileIgnored => {
                    stats.file_ignored += 1;
                    false
                }
                Verdict::Suppressed => {
                    stats.suppressed += 1;
                    false
                }
            }
        });
        stats
    }
}
