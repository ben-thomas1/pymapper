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

/// Shared confidence documentation for the reachability-based rules (scoring table:
/// `analysis::deadcode`).
const REACH_CONFIDENCE: &str = "100 minus how live the definition looks. 100 when nothing \
reaches it; 50 when reached only through a `possible` call (e.g. the unique-method fallback); 40 \
when it carries an unknown decorator that may register it; 50 for a method of a class with a base \
that is not loaded (it may override a framework method); 70 when the defining module uses \
dynamic access (`getattr`, `globals()`, `vars()`, `__getattr__`), or for a method when the module \
of a base class does (name-based dispatch); in application mode 30 when \
listed in `__all__` or re-exported; in library mode 80 for a public name outside the public API \
and 50 for a public method of a class that is not part of it (its instances may reach callers). \
With `analysis.name-fallback`, 10 when the name occurs in its own file or in a file that imports \
it or its module, 50 when it occurs anywhere else (attribute, identifier, string). Callees inherit \
the confidence of such callers. `check` reports findings at or above `min-confidence` (default \
80).";

/// Confidence documentation of PM404.
const MODULE_CONFIDENCE: &str = "100 when no live module imports it or calls into it; 50 when \
reached only through `possible` calls or when its name occurs in another file (for instance \
`importlib.import_module(\"pkg.plugins.x\")`); 40 for a module outside any regular package (it \
may be run as a script) and for modules imported only from such modules.";

/// Wordings of PM401-PM403 ([`USE_NEVER`], [`USE_ONLY_UNUSED`]).
const USE: &[&str] = &["is never used", "is only used by unused code"];
/// Wordings of PM404 ([`USE_NEVER`]: no import of the module at all, [`USE_ONLY_UNUSED`]:
/// imported, but only by unused modules).
const IMPORTED: &[&str] = &["is never imported", "is only imported by unused modules"];

/// Phrase argument: nothing refers to the definition (no call, decorator, import or subclass;
/// no import for a module).
pub const USE_NEVER: u32 = 0;
/// Phrase argument: something refers to it, but only from code that is itself unused.
pub const USE_ONLY_UNUSED: u32 = 1;

const SYMBOL_USE: &[ArgKind] = &[ArgKind::Symbol, ArgKind::Phrase(USE)];

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
        code: codes::UNUSED_FUNCTION.0,
        name: "unused-function",
        summary: "Module-level or nested function that is never used",
        message: "function `{0}` {1}",
        args: SYMBOL_USE,
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
        message: "class `{0}` {1}",
        args: SYMBOL_USE,
        explanation: "No live code instantiates, subclasses or references the class, and no \
entry-point rule (base class, decorator, name) marks it as used. While PM402 is enabled, its \
methods are not reported separately unless they look less live than the class (higher \
confidence); suppressing the class finding (inline or `per-file-ignores`) hides them too.\n\n\
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
        message: "method `{0}` {1}",
        args: SYMBOL_USE,
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
        message: "module `{0}` {1}",
        args: &[ArgKind::Symbol, ArgKind::Phrase(IMPORTED)],
        explanation: "No first-party module imports it, it is not a script or `__main__` module, \
not an entry point, and (in library mode) not part of the public package. Test modules are \
usually entry points via the `pytest` preset. Packages (`__init__.py`) are never reported. \
While PM404 is enabled, the module's defs and imports are reported only when they look less \
live than the module (higher confidence); suppressing the module finding (`ignore[PM404]` on \
line 1 or `per-file-ignores`) hides them too.\n\n\
Fix: delete the module, or declare it as an entry point.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(MODULE_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_IMPORT.0,
        name: "unused-import",
        summary: "Import that neither its module nor any importer of that module uses",
        message: "`{0}` is imported but never used",
        args: &[ArgKind::Name],
        explanation: "Unlike per-file linters, this also follows re-exports: an import counts as used \
when another module imports the name from here or accesses it as `module.name`. Imports listed \
in `__all__`, the `import x as x` idiom, `__init__.py` re-exports in library mode, star \
imports, `__future__` imports and `try`/`except ImportError` fallbacks are kept.\n\n\
Fix: delete the import, or add the name to `__all__` if it is part of the module's API.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::DeadCode,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(
            "100 when no module uses the name, minus: 30 when the importing module uses dynamic \
access (`globals()`, `vars()`, `getattr`, `__getattr__`), 40 when another module star-imports it \
(public names), 40 when another file mentions both the name and the module without importing it \
(string references such as `mock.patch(\"pkg.mod.name\")`), 30 for an application-mode \
`__init__.py` re-export, 20 in an `__init__.py` (20 more for a submodule import, often made for \
its side effects), 50 under `if TYPE_CHECKING:` (mostly used in quoted annotations, which name \
evidence sees only when the whole string is a dotted name), 10 for another conditional import."),
    },
];
