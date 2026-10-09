//! PM6xx: dependency hygiene, run by `pymapper deps --check` (not `check`; see
//! `deps::hygiene`). Import names map to distributions via dist-info `top_level.txt` /
//! `RECORD`, else a built-in name table, else a guess. Findings about a declared dependency
//! (PM602) are located in the `pyproject.toml` that declares it.

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM601 `undeclared-dependency`.
    pub const UNDECLARED_DEPENDENCY: Rule = Rule(601);
    /// PM602 `unused-dependency`.
    pub const UNUSED_DEPENDENCY: Rule = Rule(602);
    /// PM603 `private-import`.
    pub const PRIVATE_IMPORT: Rule = Rule(603);
    /// PM604 `transitive-dependency`.
    pub const TRANSITIVE_DEPENDENCY: Rule = Rule(604);
}

const MAPPING_CONFIDENCE: &str = "100 when the import name maps to a distribution through installed \
dist-info metadata (`top_level.txt`, `RECORD`), 90 through the built-in table of import names that \
differ from their distribution (`yaml` -> `PyYAML`), 60 for a guess from the name alone (below the \
default gate). An import guarded by `except ImportError` (an optional dependency) gets at most 50.";

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
        code: codes::UNDECLARED_DEPENDENCY.0,
        name: "undeclared-dependency",
        summary: "Import of a third-party distribution not declared in `pyproject.toml`",
        message: "`{0}` is provided by `{1}`, which is not a declared dependency",
        args: &[ArgKind::Name, ArgKind::Name],
        explanation: "First-party code imports a module whose distribution is not listed in \
`[project.dependencies]`, `[project.optional-dependencies]`, a `[dependency-groups]` group or the \
poetry / uv / pdm dependency tables of the `pyproject.toml` that applies to the file (its nearest \
project unit that declares dependencies). The code works only because the package happens to be \
installed. Units whose `dependencies` are `dynamic` are not checked.\n\n\
Fix: add the distribution to the project's dependencies.",
        severity: Severity::Error,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Deps,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(MAPPING_CONFIDENCE),
    },
    RuleDef {
        code: codes::UNUSED_DEPENDENCY.0,
        name: "unused-dependency",
        summary: "Declared dependency that no first-party module imports",
        message: "dependency `{0}` is declared but never imported",
        args: &[ArgKind::Name],
        explanation: "A distribution in `[project.dependencies]` (or poetry's main dependencies) \
provides no module that the unit's first-party code imports. Optional and development groups are \
not checked. A distribution that installs no importable name (a command-line tool, a \
meta-package) is reported with confidence 50. Plugins, CLI tools and runtime-only extras are \
legitimate exceptions; ignore them with `per-file-ignores` on `pyproject.toml` or an inline \
suppression on the dependency line.\n\n\
Fix: remove the dependency, or move it to the group that needs it.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Deps,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(MAPPING_CONFIDENCE),
    },
    RuleDef {
        code: codes::PRIVATE_IMPORT.0,
        name: "private-import",
        summary: "Import of a private module or name of a third-party package",
        message: "`{0}` is private to `{1}`",
        args: &[ArgKind::Name, ArgKind::Name],
        explanation: "The import path contains a component starting with an underscore \
(`pkg._internal`, `from pkg import _helper`) of a third-party distribution; dunders \
(`__version__`) and the lone `_` (the gettext idiom) are not private. Private internals change \
without notice between releases.\n\n\
Fix: use the package's public API, or pin the version and isolate the access in one module.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Deps,
        thresholds: &[],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::TRANSITIVE_DEPENDENCY.0,
        name: "transitive-dependency",
        summary: "Import of a distribution installed only as a dependency of a dependency",
        message: "`{0}` is provided by `{1}`, installed only as a dependency of `{2}`",
        args: &[ArgKind::Name, ArgKind::Name, ArgKind::Name],
        explanation: "A refinement of PM601: the imported distribution is not declared, but a \
declared dependency requires it, directly or transitively (installed `Requires-Dist` metadata, \
extras honoured). It may disappear when that dependency changes. When this rule \
is enabled such imports are reported as PM604 instead of PM601; it is off by default because \
PM601 already reports them.\n\n\
Fix: declare the distribution directly.",
        severity: Severity::Warning,
        default_enabled: false,
        cost: CostClass::Linear,
        gate: Gate::None,
        pass: Pass::Deps,
        thresholds: &[],
        confidence: ConfidenceModel::Scored(MAPPING_CONFIDENCE),
    },
];
