//! PM1xx: complexity and size (owned by the hotspot analysis). Metric definitions: design,
//! "Metric definitions". Every rule here is a measured fact (confidence 100), computed from
//! metrics recorded during extraction, so the pass itself is a linear scan over definitions.

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity, ThresholdKey};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM101 `complex-function`.
    pub const COMPLEX_FUNCTION: Rule = Rule(101);
    /// PM102 `cognitive-complexity`.
    pub const COGNITIVE_COMPLEXITY: Rule = Rule(102);
    /// PM103 `deep-nesting`.
    pub const DEEP_NESTING: Rule = Rule(103);
    /// PM104 `long-function`.
    pub const LONG_FUNCTION: Rule = Rule(104);
    /// PM105 `too-many-parameters`.
    pub const TOO_MANY_PARAMETERS: Rule = Rule(105);
    /// PM106 `low-maintainability`.
    pub const LOW_MAINTAINABILITY: Rule = Rule(106);
    /// PM107 `long-module`.
    pub const LONG_MODULE: Rule = Rule(107);
}

const FN_METRIC: &[ArgKind] = &[ArgKind::Symbol, ArgKind::U32, ArgKind::U32];

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
        code: codes::COMPLEX_FUNCTION.0,
        name: "complex-function",
        summary: "Function with a McCabe cyclomatic complexity above `max-mccabe`",
        message: "`{0}` is too complex (McCabe {1} > {2})",
        args: FN_METRIC,
        explanation: "McCabe complexity counts the independent paths through a function, exactly as \
ruff's C901 / PyCQA mccabe: 1, plus one per `if`, `elif`, `for`, `while`, `except` handler, \
`try`-`else`, nested function and `match` case (minus one when the last case is an irrefutable, \
unguarded pattern). Boolean operators, conditional expressions, comprehensions, `with` and \
`assert` are not counted. High values mean many paths to test and reason about.\n\n\
Fix: extract branches into well-named helpers, replace `if`/`elif` chains with a dispatch table, \
or return early.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxMccabe],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::COGNITIVE_COMPLEXITY.0,
        name: "cognitive-complexity",
        summary: "Function with a cognitive complexity above `max-cognitive`",
        message: "`{0}` is hard to understand (cognitive complexity {1} > {2})",
        args: FN_METRIC,
        explanation: "Cognitive complexity (SonarSource) weights control flow by how deeply it is \
nested: `if`, conditional expressions, `match`, `for`, `while` and `except` cost one plus the \
current nesting level; `elif` and `else` cost one; each run of like boolean operators costs one; \
direct recursion costs one. Nested functions and lambdas count toward their parent.\n\n\
Fix: flatten nesting with guard clauses, extract nested blocks into functions, and simplify \
boolean conditions.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxCognitive],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::DEEP_NESTING.0,
        name: "deep-nesting",
        summary: "Function whose blocks nest deeper than `max-nesting`",
        message: "`{0}` nests blocks {1} levels deep (> {2})",
        args: FN_METRIC,
        explanation: "The maximum depth of nested compound statements (`if`, `for`, `while`, `try`, \
`with`, `match`) inside a function body. Deeply nested code is hard to follow and usually mixes \
several responsibilities.\n\n\
Fix: return or `continue` early, and move inner blocks into helper functions.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxNesting],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::LONG_FUNCTION.0,
        name: "long-function",
        summary: "Function longer than `max-function-loc` lines, docstrings excluded",
        message: "`{0}` is {1} lines long without docstrings (> {2})",
        args: FN_METRIC,
        explanation: "Lines of the function's inclusive span, from its first decorator or `def` to \
its last line, minus the lines of its docstring and of the docstrings of functions and classes \
nested in it (a long numpydoc docstring is documentation, not code). Blank lines and comments \
count. A docstring is the first statement of a body when it is a plain string literal (not an \
f-string or bytes) on its own line(s). Long functions tend to do several things.\n\n\
Fix: split the function along its steps.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxFunctionLoc],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::TOO_MANY_PARAMETERS.0,
        name: "too-many-parameters",
        summary: "Function with more than `max-params` parameters",
        message: "`{0}` takes {1} parameters (> {2})",
        args: FN_METRIC,
        explanation: "Counts every parameter (positional-only, regular, `*args`, keyword-only, \
`**kwargs`) except a leading `self` / `cls` of a method. Many parameters make calls hard to \
read and suggest the function does too much.\n\n\
Fix: group related parameters into a dataclass, or split the function.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxParams],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::LOW_MAINTAINABILITY.0,
        name: "low-maintainability",
        summary: "Function with a maintainability index below `min-maintainability`",
        message: "`{0}` has a low maintainability index ({1} < {2})",
        args: &[ArgKind::Symbol, ArgKind::F32, ArgKind::F32],
        explanation: "Maintainability index on a 0-100 scale: \
`clamp((171 - 5.2 ln V - 0.23 CC - 16.2 ln LOC) * 100 / 171, 0, 100)`, where V is the Halstead \
volume, CC the McCabe complexity and LOC the line count. It blends size and complexity into one \
number and is best used to rank code, which is why the rule is off by default.\n\n\
Fix: reduce the function's size and complexity (see PM101 and PM104).",
        severity: Severity::Info,
        default_enabled: false,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MinMaintainability],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::LONG_MODULE.0,
        name: "long-module",
        summary: "Module longer than `max-module-loc` lines, docstrings excluded",
        message: "module `{0}` is {1} lines long without docstrings (> {2})",
        args: FN_METRIC,
        explanation: "Lines of the module file minus the lines of every docstring in it (the \
module's, and those of its classes and functions, as for PM104). Blank lines and comments \
count. Very long modules are hard to navigate and \
usually hold several unrelated concerns.\n\n\
Fix: split the module into a package of focused modules.",
        severity: Severity::Info,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Hotspot,
        thresholds: &[ThresholdKey::MaxModuleLoc],
        confidence: ConfidenceModel::Certain,
    },
];
