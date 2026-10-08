//! Rule framework (design: Checks / CLI): the rule registry, the [`Finding`] model, rule
//! selection and gating ([`RuleEngine`]), inline suppressions and the `docs/rules.md` generator.
//!
//! # Codes
//!
//! Rules are identified by `PM` plus three digits. The hundreds digit is the [`Category`]:
//!
//! | range | category |
//! |-------|----------|
//! | PM1xx | complexity and size |
//! | PM2xx | coupling |
//! | PM3xx | import cycles and layering contracts |
//! | PM4xx | dead code |
//! | PM5xx | duplication |
//! | PM6xx | dependencies |
//!
//! Each category's rules live in their own file (`rules/<category>.rs`), so analysis steps add or
//! change rules without touching each other. The registry is a static table validated at compile
//! time (duplicate codes and codes outside their category's range do not compile).
//!
//! # Life of a finding
//!
//! 1. [`RuleEngine::new`] resolves `select` / `ignore` / analysis toggles once per run into bit
//!    sets. Analyses ask [`RuleEngine::should_run`] before doing any work for a [`Pass`] and
//!    [`RuleEngine::is_enabled`] before emitting a rule, so disabled and gated rules cost nothing.
//! 2. Analyses push [`Finding`]s into per-thread [`Findings`] buffers with typed [`Arg`]s; no
//!    message text is formatted. Buffers merge by concatenation ([`Findings::append`]).
//! 3. [`RuleEngine::filter`] drops findings of disabled rules, `per-file-ignores`, inline
//!    `# pymapper: ignore[...]` suppressions ([`Suppressions`]) and (optionally) findings below
//!    `min-confidence`, compacting the buffer in place.
//! 4. Output formats messages on demand with [`RuleDef::write_message`] into a reused buffer.

mod complexity;
mod coupling;
mod dead_code;
mod dependencies;
mod docs;
mod duplication;
mod engine;
mod finding;
mod layering;
mod ruleset;
mod suppress;

#[cfg(test)]
mod tests;

use std::fmt;

use crate::config::{AnalysisConfig, Thresholds};

pub use docs::render_markdown;
pub use engine::{FilterStats, RuleEngine, Verdict};
pub use finding::{Arg, ArgKind, ArgResolver, FileId, Finding, Findings, Location, MessageError, SymbolId};
pub use ruleset::RuleSet;
pub use suppress::{BadCode, BadCodeKind, SuppressionEntry, Suppressions, SuppressionsBuilder, parse_codes};

/// A rule, by its numeric code (`Rule(101)` is `PM101`). Always a registered code when obtained
/// from the registry or a `codes` constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rule(pub u16);

impl Rule {
    /// The registry entry. Panics if the code is not registered (a programming error: rules
    /// are only created from registered constants).
    #[inline]
    pub fn def(self) -> &'static RuleDef {
        match lookup(self.0) {
            Some(def) => def,
            None => panic!("unregistered rule code PM{:03}", self.0),
        }
    }

    /// The numeric code.
    #[inline]
    pub fn code(self) -> u16 {
        self.0
    }

    /// The category from the hundreds digit, if it is a reserved range.
    #[inline]
    pub fn category(self) -> Option<Category> {
        Category::of(self.0)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PM{:03}", self.0)
    }
}

/// Reserved code ranges (the hundreds digit of the code).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// PM1xx: per-function and per-module complexity and size metrics.
    Complexity = 1,
    /// PM2xx: coupling between modules.
    Coupling = 2,
    /// PM3xx: import cycles and import (layering) contracts.
    Layering = 3,
    /// PM4xx: dead code.
    DeadCode = 4,
    /// PM5xx: duplicated code.
    Duplication = 5,
    /// PM6xx: dependency hygiene.
    Dependencies = 6,
}

impl Category {
    /// Every category, in code order.
    pub const ALL: &[Category] = &[
        Category::Complexity,
        Category::Coupling,
        Category::Layering,
        Category::DeadCode,
        Category::Duplication,
        Category::Dependencies,
    ];

    /// The category of a code, if its range is reserved.
    pub const fn of(code: u16) -> Option<Category> {
        match code / 100 {
            1 => Some(Category::Complexity),
            2 => Some(Category::Coupling),
            3 => Some(Category::Layering),
            4 => Some(Category::DeadCode),
            5 => Some(Category::Duplication),
            6 => Some(Category::Dependencies),
            _ => None,
        }
    }

    /// The hundreds digit.
    pub const fn digit(self) -> u16 {
        self as u16
    }

    /// Human-readable name.
    pub fn title(self) -> &'static str {
        match self {
            Category::Complexity => "Complexity and size",
            Category::Coupling => "Coupling",
            Category::Layering => "Import cycles and layering",
            Category::DeadCode => "Dead code",
            Category::Duplication => "Duplication",
            Category::Dependencies => "Dependencies",
        }
    }
}

/// How serious a finding is. Maps to SARIF `level` (`error` / `warning` / `note`) and GitHub
/// annotation kinds. Severity does not affect the exit code; confidence does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational.
    Info,
    /// Likely a problem.
    Warning,
    /// Breaks an explicit contract or is almost certainly a defect.
    Error,
}

impl Severity {
    /// Lower-case name (`info`, `warning`, `error`).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// Asymptotic cost of the work a rule needs (design: Performance rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CostClass {
    /// Linear in the size of the code (definitions, imports, tokens); usually computed during
    /// extraction.
    Linear,
    /// Linear in the size of a graph (`O(V + E)`).
    Graph,
    /// Superlinear or memory-hungry; only runs when its [`Gate`] is open.
    Expensive,
}

impl CostClass {
    /// Documentation spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CostClass::Linear => "O(n)",
            CostClass::Graph => "O(V+E)",
            CostClass::Expensive => "expensive",
        }
    }
}

/// An `[analysis]` toggle that must be on for an expensive rule (or pass) to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gate {
    /// Not gated.
    None,
    /// `analysis.dupes`.
    Dupes,
}

impl Gate {
    /// Whether the toggle is on.
    #[inline]
    pub fn is_open(self, analysis: &AnalysisConfig) -> bool {
        match self {
            Gate::None => true,
            Gate::Dupes => analysis.dupes,
        }
    }

    /// The config key of the toggle (`None` for [`Gate::None`]).
    pub fn key(self) -> Option<&'static str> {
        match self {
            Gate::None => None,
            Gate::Dupes => Some("analysis.dupes"),
        }
    }
}

/// An analysis pass. A pass runs when at least one rule it produces is enabled; ask
/// [`RuleEngine::should_run`] before doing any of its work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pass {
    /// `analysis/hotspot`: metric thresholds per function and module.
    Hotspot = 0,
    /// `analysis/coupling`: fan-in/fan-out and related module metrics.
    Coupling = 1,
    /// `analysis/cycles`: strongly connected components of the import graph.
    Cycles = 2,
    /// `analysis/layers`: import contracts.
    Layers = 3,
    /// `analysis/deadcode`: reachability from roots.
    DeadCode = 4,
    /// `analysis/dupes`: clone detection.
    Dupes = 5,
    /// `deps`: dependency hygiene.
    Deps = 6,
}

impl Pass {
    /// Every pass.
    pub const ALL: &[Pass] =
        &[Pass::Hotspot, Pass::Coupling, Pass::Cycles, Pass::Layers, Pass::DeadCode, Pass::Dupes, Pass::Deps];

    /// Bit in a pass mask.
    #[inline]
    pub(crate) const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// Whether a threshold is an upper or a lower bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// Raise when the value is strictly greater than the limit.
    Max,
    /// Raise when the value is strictly less than the limit.
    Min,
    /// A detection parameter (smallest reported duplicate), not a pass/fail limit.
    Param,
}

/// A `[thresholds]` key a rule reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThresholdKey {
    /// `max-mccabe`.
    MaxMccabe,
    /// `max-cognitive`.
    MaxCognitive,
    /// `max-nesting`.
    MaxNesting,
    /// `max-function-loc`.
    MaxFunctionLoc,
    /// `max-params`.
    MaxParams,
    /// `min-maintainability`.
    MinMaintainability,
    /// `max-module-loc`.
    MaxModuleLoc,
    /// `max-fan-out`.
    MaxFanOut,
    /// `max-distance`.
    MaxDistance,
    /// `distance-min-coupling`.
    DistanceMinCoupling,
    /// `max-module-symbols`.
    MaxModuleSymbols,
    /// `max-module-coupling`.
    MaxModuleCoupling,
    /// `max-class-wmc`.
    MaxClassWmc,
    /// `max-class-fan-out`.
    MaxClassFanOut,
    /// `dupes-min-tokens`.
    DupesMinTokens,
    /// `dupes-min-lines`.
    DupesMinLines,
}

impl ThresholdKey {
    /// The config key, under `[thresholds]`.
    pub fn key(self) -> &'static str {
        match self {
            ThresholdKey::MaxMccabe => "max-mccabe",
            ThresholdKey::MaxCognitive => "max-cognitive",
            ThresholdKey::MaxNesting => "max-nesting",
            ThresholdKey::MaxFunctionLoc => "max-function-loc",
            ThresholdKey::MaxParams => "max-params",
            ThresholdKey::MinMaintainability => "min-maintainability",
            ThresholdKey::MaxModuleLoc => "max-module-loc",
            ThresholdKey::MaxFanOut => "max-fan-out",
            ThresholdKey::MaxDistance => "max-distance",
            ThresholdKey::DistanceMinCoupling => "distance-min-coupling",
            ThresholdKey::MaxModuleSymbols => "max-module-symbols",
            ThresholdKey::MaxModuleCoupling => "max-module-coupling",
            ThresholdKey::MaxClassWmc => "max-class-wmc",
            ThresholdKey::MaxClassFanOut => "max-class-fan-out",
            ThresholdKey::DupesMinTokens => "dupes-min-tokens",
            ThresholdKey::DupesMinLines => "dupes-min-lines",
        }
    }

    /// Upper bound, lower bound or detection parameter.
    pub fn bound(self) -> Bound {
        match self {
            ThresholdKey::MinMaintainability => Bound::Min,
            ThresholdKey::DupesMinTokens | ThresholdKey::DupesMinLines | ThresholdKey::DistanceMinCoupling => {
                Bound::Param
            }
            _ => Bound::Max,
        }
    }

    /// The configured value (exact: every integer threshold fits in an `f64`).
    pub fn get(self, t: &Thresholds) -> f64 {
        match self {
            ThresholdKey::MaxMccabe => f64::from(t.max_mccabe),
            ThresholdKey::MaxCognitive => f64::from(t.max_cognitive),
            ThresholdKey::MaxNesting => f64::from(t.max_nesting),
            ThresholdKey::MaxFunctionLoc => f64::from(t.max_function_loc),
            ThresholdKey::MaxParams => f64::from(t.max_params),
            ThresholdKey::MinMaintainability => t.min_maintainability,
            ThresholdKey::MaxModuleLoc => f64::from(t.max_module_loc),
            ThresholdKey::MaxFanOut => f64::from(t.max_fan_out),
            ThresholdKey::MaxDistance => t.max_distance,
            ThresholdKey::DistanceMinCoupling => f64::from(t.distance_min_coupling),
            ThresholdKey::MaxModuleSymbols => f64::from(t.max_module_symbols),
            ThresholdKey::MaxModuleCoupling => f64::from(t.max_module_coupling),
            ThresholdKey::MaxClassWmc => f64::from(t.max_class_wmc),
            ThresholdKey::MaxClassFanOut => f64::from(t.max_class_fan_out),
            ThresholdKey::DupesMinTokens => f64::from(t.dupes_min_tokens),
            ThresholdKey::DupesMinLines => f64::from(t.dupes_min_lines),
        }
    }

    /// Whether `value` violates the limit: `> limit` for [`Bound::Max`], `< limit` for
    /// [`Bound::Min`]; [`Bound::Param`] keys never "violate" (use [`get`](Self::get)).
    #[inline]
    pub fn violated_by(self, t: &Thresholds, value: f64) -> bool {
        match self.bound() {
            Bound::Max => value > self.get(t),
            Bound::Min => value < self.get(t),
            Bound::Param => false,
        }
    }
}

/// What a finding's confidence (0-100) means for a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfidenceModel {
    /// A measured fact: every finding has confidence 100.
    Certain,
    /// Scored per finding; the text documents how (shown in `docs/rules.md`).
    Scored(&'static str),
}

/// A registered rule. All fields are static data; see the category files for the entries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuleDef {
    /// Numeric code (`101` for `PM101`).
    pub code: u16,
    /// Kebab-case name, unique (`complex-function`).
    pub name: &'static str,
    /// One-line description.
    pub summary: &'static str,
    /// Message template. `{N}` is argument N; `{N..SEP}` is arguments N.. joined by `SEP`.
    pub message: &'static str,
    /// Argument kinds, in order. With a `{N..SEP}` placeholder, the last kind repeats.
    pub args: &'static [ArgKind],
    /// Longer Markdown explanation: what is detected, why it matters, how to fix it.
    pub explanation: &'static str,
    /// Severity of every finding.
    pub severity: Severity,
    /// Enabled when `select` is unset (expensive rules additionally need their gate).
    pub default_enabled: bool,
    /// Cost class of the work behind the rule.
    pub cost: CostClass,
    /// Toggle required to run; [`Gate::None`] unless `cost` is [`CostClass::Expensive`].
    pub gate: Gate,
    /// The pass that produces the rule.
    pub pass: Pass,
    /// Thresholds read by the rule.
    pub thresholds: &'static [ThresholdKey],
    /// Meaning of the confidence score.
    pub confidence: ConfidenceModel,
}

impl RuleDef {
    /// The rule handle.
    #[inline]
    pub fn rule(&self) -> Rule {
        Rule(self.code)
    }
}

/// Rule groups, one per category, in code order. Each group is sorted by code.
const GROUPS: &[&[RuleDef]] = &[
    complexity::RULES,
    coupling::RULES,
    layering::RULES,
    dead_code::RULES,
    duplication::RULES,
    dependencies::RULES,
];

/// Number of addressable codes (`PM000`..`PM999`).
pub const CODE_SPACE: usize = 1000;

/// Code -> rule, built and validated at compile time.
static BY_CODE: [Option<&'static RuleDef>; CODE_SPACE] = build_table(GROUPS);

const fn build_table(groups: &'static [&'static [RuleDef]]) -> [Option<&'static RuleDef>; CODE_SPACE] {
    let mut table: [Option<&'static RuleDef>; CODE_SPACE] = [None; CODE_SPACE];
    let mut g = 0;
    while g < groups.len() {
        let group = groups[g];
        let mut i = 0;
        let mut prev = 0u16;
        while i < group.len() {
            let def = &group[i];
            let code = def.code;
            assert!((code as usize) < CODE_SPACE, "rule code out of range");
            assert!(!code.is_multiple_of(100),"codes ending in 00 are reserved");
            assert!(code > prev, "rule groups must be sorted by code");
            match Category::of(code) {
                Some(cat) => assert!(cat as usize == g + 1, "rule code outside its category's range"),
                None => panic!("rule code outside the reserved ranges"),
            }
            assert!(table[code as usize].is_none(), "duplicate rule code");
            let expensive = matches!(def.cost, CostClass::Expensive);
            let gated = !matches!(def.gate, Gate::None);
            assert!(expensive == gated, "expensive rules (and only those) must declare a gate");
            table[code as usize] = Some(def);
            prev = code;
            i += 1;
        }
        g += 1;
    }
    table
}

/// The registry entry for a numeric code.
#[inline]
pub fn lookup(code: u16) -> Option<&'static RuleDef> {
    BY_CODE.get(code as usize).copied().flatten()
}

/// The registry entry for a code spelled `PMxxx` (exact, case-sensitive).
pub fn lookup_code(text: &str) -> Option<&'static RuleDef> {
    let digits = text.strip_prefix("PM")?.as_bytes();
    if digits.len() != 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let code = digits.iter().fold(0u16, |acc, &b| acc * 10 + u16::from(b - b'0'));
    lookup(code)
}

/// The registry entry with this kebab-case name. Linear over the (small, static) registry;
/// for CLI lookups, not hot paths.
pub fn lookup_name(name: &str) -> Option<&'static RuleDef> {
    all().find(|d| d.name == name)
}

/// Every registered rule, in code order.
pub fn all() -> impl Iterator<Item = &'static RuleDef> {
    GROUPS.iter().flat_map(|g| g.iter())
}

/// Rules of one category, in code order.
pub fn in_category(cat: Category) -> &'static [RuleDef] {
    GROUPS[cat as usize - 1]
}

/// Rule handles for analysis code, e.g. `codes::COMPLEX_FUNCTION`.
pub mod codes {
    pub use super::complexity::codes::*;
    pub use super::coupling::codes::*;
    pub use super::dead_code::codes::*;
    pub use super::dependencies::codes::*;
    pub use super::duplication::codes::*;
    pub use super::layering::codes::*;
}
