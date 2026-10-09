//! PM3xx: import cycles and import contracts (owned by the cycles and layers analyses).
//! Contracts are configured as `[[contracts]]` (design: layering contracts).

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM301 `import-cycle`.
    pub const IMPORT_CYCLE: Rule = Rule(301);
    /// PM302 `layer-violation`.
    pub const LAYER_VIOLATION: Rule = Rule(302);
    /// PM303 `forbidden-import`.
    pub const FORBIDDEN_IMPORT: Rule = Rule(303);
    /// PM304 `independence-violation`.
    pub const INDEPENDENCE_VIOLATION: Rule = Rule(304);
}

const CONTRACT_EDGE: &[ArgKind] = &[ArgKind::Symbol, ArgKind::Symbol, ArgKind::Contract];

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
        code: codes::IMPORT_CYCLE.0,
        name: "import-cycle",
        summary: "Modules that import each other, directly or indirectly",
        message: "import cycle ({0} {1}): {2.. -> }",
        args: &[ArgKind::U32, ArgKind::Phrase(&["module", "modules"]), ArgKind::Symbol],
        explanation: "One finding per strongly connected component of the first-party module import \
graph, reported at an import that closes a short witness cycle. The message gives the number of \
modules in the component and the witness, ending at the first module again; JSON output adds a \
`cycle` object with the component's `size`, every `member` and the `witness`. Cycles make import order matter, cause `ImportError`s on partially \
initialised modules and prevent splitting code into independent parts. Imports under \
`if TYPE_CHECKING:` are ignored unless `analysis.type-checking-cycles` is set; function-level \
imports count, stub files (`.pyi`) do not.\n\n\
Fix: move the shared code into a lower-level module both can import, or invert the dependency.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Cycles,
        thresholds: &[],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::LAYER_VIOLATION.0,
        name: "layer-violation",
        summary: "Import from a lower layer into a higher one, breaking a `layers` contract",
        message: "`{0}` imports higher layer `{1}` (contract `{2}`)",
        args: CONTRACT_EDGE,
        explanation: "A `kind = \"layers\"` contract lists layers from highest to lowest: a layer may \
import lower layers, never higher ones. With `containers`, each container has its own layers and \
imports between containers are not checked. Reported at each violating import (sibling imports \
under `independent-siblings` are PM304). With `transitive = true`, an import of a module outside \
the contract that leads to a higher layer through other such modules counts too; the message \
names the higher-layer module reached. Stub files (`.pyi`) are not checked.\n\n\
Fix: move the imported code down a layer, or pass it in from the higher layer (dependency \
injection, callbacks).",
        severity: Severity::Error,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Layers,
        thresholds: &[],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::FORBIDDEN_IMPORT.0,
        name: "forbidden-import",
        summary: "Import breaking a `forbidden` contract",
        message: "`{0}` imports forbidden module `{1}` (contract `{2}`)",
        // The imported module may be third-party and outside the analysed set: a dotted name.
        args: &[ArgKind::Symbol, ArgKind::Name, ArgKind::Contract],
        explanation: "A `kind = \"forbidden\"` contract forbids its `source` modules (and their \
submodules) from importing its `forbidden` modules, which may be first- or third-party. Modules \
that are not analysed match unresolved imports by dotted prefix. Reported at each violating \
import; with `transitive = true` also at imports that lead to a forbidden module through modules \
outside the contract. Stub files (`.pyi`) are not checked.\n\n\
Fix: remove the dependency, or move the code that needs it out of the source modules.",
        severity: Severity::Error,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Layers,
        thresholds: &[],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::INDEPENDENCE_VIOLATION.0,
        name: "independence-violation",
        summary: "Import between modules of an `independence` contract",
        message: "`{0}` imports `{1}`, which must stay independent (contract `{2}`)",
        args: CONTRACT_EDGE,
        explanation: "A `kind = \"independence\"` contract lists modules that may not import one \
another (submodules included); the same applies to siblings within a layer of a `layers` \
contract with `independent-siblings`. Reported at each violating import; with \
`transitive = true` also at imports that lead to another listed module through modules outside \
the contract. Stub files (`.pyi`) are not checked.\n\n\
Fix: move the shared code into a separate module that both may import.",
        severity: Severity::Error,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Layers,
        thresholds: &[],
        confidence: ConfidenceModel::Certain,
    },
];
