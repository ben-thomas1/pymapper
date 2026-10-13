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
it or its module, 50 when it occurs anywhere else (attribute, identifier, string); every \
first-party `.py` file counts, importable or not (tests in hyphenated directories). When only test \
code mentions the name, the same 10 or 50 with the reason `used-only-in-tests` (test-only API is \
not dead). Callees inherit the confidence of such callers. With a coverage report \
(`analysis.coverage`, `--coverage`), code that ran is never reported (nor what it calls), and the \
doubt of code that never ran is divided by 3 (50 becomes 84, 80 becomes 94; reason \
`never-executed`). `check` reports findings at or above `min-confidence` (default 80).";

/// Confidence documentation of PM404.
const MODULE_CONFIDENCE: &str = "100 when no live module imports it or calls into it; 50 when \
reached only through `possible` calls or when its name occurs in another file (for instance \
`importlib.import_module(\"pkg.plugins.x\")`; `used-only-in-tests` when only test code names it); \
40 for a module outside any regular package (it \
may be run as a script) and for modules imported only from such modules. With a coverage report, a \
module that was imported is never reported and the doubt of one that never was is divided by 3 \
(40 becomes 80; reason `never-executed`).";

/// Wordings of PM401-PM403, indexed by the `USE_*` phrase constants. "is never used" only when
/// nothing refers to the definition at all; otherwise the wording names the strongest evidence
/// that keeps the confidence below 100 (the `reasons` of the finding carry the full set).
const USE: &[&str] = &[
    "is never used",
    "is only used by unused code",
    "is only used by code that may be unused",
    "has no resolved callers (name appears elsewhere)",
    "has no resolved callers (name appears in its own file)",
    "has no resolved callers (name appears in a file that imports it)",
    "has only possible callers (unique method name, unresolved receiver)",
    "has no resolved callers (an unknown decorator may register it)",
    "has no resolved callers (may override a method of an unloaded base class)",
    "has no resolved callers (dynamic attribute access may reach it)",
    "has no resolved callers (matches a dynamic `getattr` prefix)",
    "is only exported (`__all__` or a re-export), never used",
    "is never used (public name outside the public API)",
    "is never used (public method; instances may reach callers)",
    "has no resolved callers (name appears in a config file)",
    "has no resolved callers (named with its module in a config file)",
    "has no resolved callers (name appears only in tests)",
];
/// Wordings of PM404, indexed by the `IMP_*` phrase constants.
const IMPORTED: &[&str] = &[
    "is never imported",
    "is only imported by unused modules",
    "is only imported by modules that may be unused",
    "is not imported (name appears elsewhere)",
    "is only reached through possible calls",
    "is not imported (may be run as a script)",
    "is not imported (name appears in a config file)",
    "is not imported (name appears only in tests)",
];

/// Phrase argument: nothing refers to the definition (no call, decorator, import or subclass;
/// no import for a module).
pub const USE_NEVER: u32 = 0;
/// Phrase argument: something refers to it, but only from code that is itself unused.
pub const USE_ONLY_UNUSED: u32 = 1;
/// Phrase argument: only reached from code that is itself in doubt (confidence below 100).
pub const USE_MAYBE_UNUSED: u32 = 2;
pub const USE_NAME_ELSEWHERE: u32 = 3;
pub const USE_NAME_SAME_FILE: u32 = 4;
pub const USE_NAME_IMPORTER: u32 = 5;
pub const USE_POSSIBLE: u32 = 6;
pub const USE_DECORATED: u32 = 7;
pub const USE_EXTERNAL_BASE: u32 = 8;
pub const USE_DYNAMIC: u32 = 9;
pub const USE_DYNAMIC_PREFIX: u32 = 10;
pub const USE_EXPORTED: u32 = 11;
pub const USE_LIBRARY_PUBLIC: u32 = 12;
pub const USE_API_METHOD: u32 = 13;
pub const USE_CONFIG_NAME: u32 = 14;
pub const USE_CONFIG_LINKED: u32 = 15;
/// Phrase argument: its name occurs only in test code.
pub const USE_TESTS: u32 = 16;

/// PM404 phrases (see [`IMPORTED`]).
pub const IMP_NEVER: u32 = 0;
pub const IMP_ONLY_UNUSED: u32 = 1;
pub const IMP_MAYBE_UNUSED: u32 = 2;
pub const IMP_NAME_ELSEWHERE: u32 = 3;
pub const IMP_POSSIBLE: u32 = 4;
pub const IMP_SCRIPT_LIKE: u32 = 5;
pub const IMP_CONFIG_NAME: u32 = 6;
pub const IMP_TESTS: u32 = 7;

/// Confidence reasons of PM401-PM405 findings ([`super::Finding::reasons`] bits).
pub mod reason {
    /// Nothing refers to it at all.
    pub const NO_REFERENCES: u32 = 1 << 0;
    /// Referenced (call, decorator, import, subclass) only from code that is itself in doubt.
    pub const ONLY_USED_BY_UNUSED_CODE: u32 = 1 << 1;
    /// Its name occurs in its own file.
    pub const NAME_IN_SAME_FILE: u32 = 1 << 2;
    /// Its name occurs in a file that imports it or its module.
    pub const NAME_IN_IMPORTER: u32 = 1 << 3;
    /// Its name occurs in some other file.
    pub const NAME_ELSEWHERE: u32 = 1 << 4;
    /// Reached only through `possible` calls (unique-method fallback).
    pub const POSSIBLE_CALL_ONLY: u32 = 1 << 5;
    /// Its module uses dynamic access (variable `getattr`, `globals()`, `__getattr__`, ...).
    pub const MODULE_DYNAMIC_ACCESS: u32 = 1 << 6;
    /// A loaded base class's module uses dynamic access (name-based dispatch).
    pub const DYNAMIC_BASE: u32 = 1 << 7;
    /// Its name starts with the constant prefix of a dynamic `getattr` (`f"_dt_{x}"`).
    pub const GETATTR_PREFIX: u32 = 1 << 8;
    /// Carries a decorator that may register it.
    pub const UNKNOWN_DECORATOR: u32 = 1 << 9;
    /// Method of a class with a base that is not loaded.
    pub const EXTERNAL_BASE: u32 = 1 << 10;
    /// Application mode: listed in `__all__` or re-exported.
    pub const EXPORTED: u32 = 1 << 11;
    /// Library mode: a public name outside the public API (not in `__all__`, private module).
    pub const LIBRARY_PUBLIC_NOT_IN_ALL: u32 = 1 << 12;
    /// Library mode: a public method of a class outside the public API.
    pub const LIBRARY_API_METHOD: u32 = 1 << 13;
    /// Named in a non-Python config file (YAML, TOML, JSON, ...).
    pub const CONFIG_REFERENCE: u32 = 1 << 14;
    /// Its dotted path occurs in a string literal.
    pub const STRING_REFERENCE: u32 = 1 << 15;
    /// A module outside any regular package (may be run as a script).
    pub const SCRIPT_LIKE: u32 = 1 << 16;
    /// PM405: another module star-imports the file.
    pub const STAR_IMPORTED: u32 = 1 << 17;
    /// PM405: an application-mode `__init__.py` re-export.
    pub const INIT_REEXPORT: u32 = 1 << 18;
    /// PM405: in an `__init__.py`.
    pub const PACKAGE_INIT: u32 = 1 << 19;
    /// PM405: a submodule import in an `__init__.py` (often for side effects).
    pub const INIT_SUBMODULE: u32 = 1 << 20;
    /// PM405: under `if TYPE_CHECKING:`.
    pub const TYPE_CHECKING_IMPORT: u32 = 1 << 21;
    /// PM405: another conditional import.
    pub const CONDITIONAL_IMPORT: u32 = 1 << 22;
    /// Overrides a method of a third-party or stdlib base class (a framework hook).
    pub const EXTERNAL_OVERRIDE: u32 = 1 << 23;
    /// Its name occurs only in test code (test files, importable or not): test-only API.
    pub const USED_ONLY_IN_TESTS: u32 = 1 << 24;
    /// The coverage report (`analysis.coverage`) shows that its code never ran.
    pub const NEVER_EXECUTED: u32 = 1 << 25;

    /// Names of the bits, lowest first (as rendered in output).
    pub const NAMES: &[&str] = &[
        "no-references",
        "only-used-by-unused-code",
        "name-in-same-file",
        "name-in-importer",
        "name-elsewhere",
        "possible-call-only",
        "module-dynamic-access",
        "dynamic-base",
        "getattr-prefix",
        "unknown-decorator",
        "external-base",
        "exported",
        "library-public-not-in-all",
        "library-api-method",
        "config-reference",
        "string-reference",
        "script-like",
        "star-imported",
        "init-reexport",
        "package-init",
        "init-submodule",
        "type-checking-import",
        "conditional-import",
        "external-override",
        "used-only-in-tests",
        "never-executed",
    ];
}

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
        fix: "delete `{0}` if nothing uses it by name, or declare how it is used (`[entrypoints]`, a preset, `__all__`)",
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
        fix: "delete `{0}` if nothing uses it by name, or declare how it is used (`[entrypoints]`, a preset, `__all__`)",
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
        fix: "delete `{0}` if nothing calls it by name, or declare how it is used",
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
        fix: "delete module `{0}` if nothing loads it by name, or declare it as an entry point",
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
        fix: "delete the import of `{0}`, or add `{0}` to `__all__` if it is part of the module's API",
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
