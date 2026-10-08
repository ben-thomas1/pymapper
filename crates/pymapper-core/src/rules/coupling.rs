//! PM2xx: coupling between modules (owned by the coupling analysis).

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity, ThresholdKey};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM201 `high-fan-out`.
    pub const HIGH_FAN_OUT: Rule = Rule(201);
}

pub(super) const RULES: &[RuleDef] = &[RuleDef {
    code: codes::HIGH_FAN_OUT.0,
    name: "high-fan-out",
    summary: "Module importing more than `max-fan-out` distinct first-party modules",
    message: "module `{0}` imports {1} first-party modules (> {2})",
    args: &[ArgKind::Symbol, ArgKind::U32, ArgKind::U32],
    explanation: "Efferent coupling: the number of distinct first-party modules a module imports \
(at any scope, `if TYPE_CHECKING:` imports excluded). A module that depends on many others breaks \
whenever any of them changes and is hard to test in isolation.\n\n\
Fix: move unrelated responsibilities into separate modules, or depend on a narrower facade.",
    severity: Severity::Warning,
    default_enabled: true,
    cost: CostClass::Graph,
    gate: Gate::None,
    pass: Pass::Coupling,
    thresholds: &[ThresholdKey::MaxFanOut],
    confidence: ConfidenceModel::Certain,
}];
