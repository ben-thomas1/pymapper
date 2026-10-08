//! PM5xx: duplicated code (owned by the dupes analysis). Expensive: gated by `analysis.dupes`.

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity, ThresholdKey};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM501 `duplicate-code`.
    pub const DUPLICATE_CODE: Rule = Rule(501);
}

pub(super) const RULES: &[RuleDef] = &[RuleDef {
    code: codes::DUPLICATE_CODE.0,
    name: "duplicate-code",
    summary: "Block of code duplicated elsewhere (token-based clone)",
    message: "{0} lines duplicated in {1}:{2}",
    args: &[ArgKind::U32, ArgKind::File, ArgKind::U32],
    explanation: "Token sequences of at least `dupes-min-tokens` tokens spanning at least \
`dupes-min-lines` lines that occur more than once, after normalising identifiers and literals \
(so renamed copies match). Reported at each copy, pointing at another copy. Duplicates must be \
kept in sync by hand and drift apart.\n\n\
Fix: extract the shared code into a function or base class.\n\n\
Off unless `analysis.dupes = true` (token-window hashing across all first-party files).",
    severity: Severity::Info,
    default_enabled: true,
    cost: CostClass::Expensive,
    gate: Gate::Dupes,
    pass: Pass::Dupes,
    thresholds: &[ThresholdKey::DupesMinTokens, ThresholdKey::DupesMinLines],
    confidence: ConfidenceModel::Scored(
        "Similarity of the two copies: 100 when their tokens are identical, lower when they only match \
after normalising identifiers and literals.",
    ),
}];
