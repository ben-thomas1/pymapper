//! PM4xx: dead code (owned by the dead-code analysis; design: Dead code (a)-(i)). A definition
//! is dead when it is not reachable from any root (module bodies, `__main__` guards, entry
//! points, presets, public API in library mode, protocol methods, overrides of live methods).

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM401 `unused-function`.
    pub const UNUSED_FUNCTION: Rule = Rule(401);
    /// PM402 `unused-class`.
    pub const UNUSED_CLASS: Rule = Rule(402);
    /// PM403 `unused-method`.
    pub const UNUSED_METHOD: Rule = Rule(403);
    /// PM404 `unused-module`.
    pub const UNUSED_MODULE: Rule = Rule(404);
    /// PM405 `unused-import`.
    pub const UNUSED_IMPORT: Rule = Rule(405);
}

/// Shared confidence documentation for the reachability-based rules.
const REACH_CONFIDENCE: &str = "Starts at 100 for a definition with no incoming reference at all and \
is lowered by weaker evidence of use: a call resolved only with `possible` confidence (e.g. the \
unique-method-name fallback), the name appearing as an attribute or identifier-like string \
elsewhere (`analysis.name-fallback`), dynamic access in the defining module (`getattr`, \
`globals()`, `__getattr__`), or a public name in library mode. Coverage evidence (when \
configured) raises it for never-executed code and suppresses executed code. `check` reports \
findings at or above `min-confidence` (default 80).";

const SYMBOL: &[ArgKind] = &[ArgKind::Symbol];

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
        code: codes::UNUSED_FUNCTION.0,
        name: "unused-function",
        summary: "Module-level or nested function that is never used",
        message: "function `{0}` is never used",
        args: SYMBOL,
        explanation: "The function is not reachable from any root: nothing live calls it, references \
it, re-exports it or registers it (decorator entry points, presets).\n\n\
Fix: delete it, or declare how it is used (`[entrypoints]`, a preset, `__all__`).",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(REACH_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_CLASS.0,
        name: "unused-class",
        summary: "Class that is never instantiated, subclassed or referenced",
        message: "class `{0}` is never used",
        args: SYMBOL,
        explanation: "No live code instantiates, subclasses or references the class, and no \
entry-point rule (base class, decorator, name) marks it as used. Its methods are not reported \
separately.\n\n\
Fix: delete it, or declare how it is used (`[entrypoints]`, a preset, `__all__`).",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(REACH_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_METHOD.0,
        name: "unused-method",
        summary: "Method of a live class that is never called",
        message: "method `{0}` is never used",
        args: SYMBOL,
        explanation: "The class is live but this method is never called through the class, its \
instances, `self` / `cls` / `super()`, or as an override of a live method in a base class. \
Dunder and protocol methods are always live.\n\n\
Fix: delete it, or declare how it is used.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(REACH_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_MODULE.0,
        name: "unused-module",
        summary: "First-party module that nothing imports and that is not an entry point",
        message: "module `{0}` is never imported",
        args: SYMBOL,
        explanation: "No first-party module imports it, it is not a script or `__main__` module, \
not an entry point, and (in library mode) not part of the public package. Test modules are \
usually entry points via the `pytest` preset.\n\n\
Fix: delete the module, or declare it as an entry point.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(REACH_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_IMPORT.0,
        name: "unused-import",
        summary: "Import that neither its module nor any importer of that module uses",
        message: "`{0}` is imported but never used",
        args: &[ArgKind::Name],
        explanation: "Unlike per-file linters, this also follows re-exports: an import counts as used \
when another module imports the name from here. Imports listed in `__all__`, `__init__.py` \
re-exports in library mode, `try`/`except ImportError` fallbacks and side-effect imports of \
known packages are kept.\n\n\
Fix: delete the import, or add the name to `__all__` if it is part of the module's API.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(
            "100 when no module uses the name; lowered when the importing module uses dynamic access \
(`globals()`, `__getattr__`, star re-exports) or the name is referenced as a string.",
        ),
    },
];
