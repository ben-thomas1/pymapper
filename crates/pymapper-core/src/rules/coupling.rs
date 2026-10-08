//! PM2xx: coupling between modules and classes (owned by the coupling analysis,
//! [`crate::analysis::coupling`]). Module rules read the module import graph (`O(V+E)`); the
//! god-class rule also reads the call graph.

use super::{ArgKind, ConfidenceModel, CostClass, Gate, Pass, Rule, RuleDef, Severity, ThresholdKey};

/// Rule handles.
pub mod codes {
    use super::Rule;

    /// PM201 `high-fan-out`.
    pub const HIGH_FAN_OUT: Rule = Rule(201);
    /// PM202 `main-sequence-distance`.
    pub const MAIN_SEQUENCE_DISTANCE: Rule = Rule(202);
    /// PM203 `god-module`.
    pub const GOD_MODULE: Rule = Rule(203);
    /// PM204 `god-class`.
    pub const GOD_CLASS: Rule = Rule(204);
}

pub(super) const RULES: &[RuleDef] = &[
    RuleDef {
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
    },
    RuleDef {
        code: codes::MAIN_SEQUENCE_DISTANCE.0,
        name: "main-sequence-distance",
        summary: "Coupled module far from the main sequence (distance above `max-distance`)",
        message: "module `{0}` is far from the main sequence (D {1} > {2}; abstractness {3}, instability {4})",
        args: &[ArgKind::Symbol, ArgKind::F32, ArgKind::F32, ArgKind::F32, ArgKind::F32],
        explanation: "Robert C. Martin's package metrics, per module. Afferent coupling Ca is the \
number of distinct first-party modules importing the module, efferent coupling Ce the number it \
imports (`if TYPE_CHECKING:` imports excluded). Instability is `I = Ce / (Ca + Ce)`; abstractness \
`A` is the share of its classes that are abstract (an `ABC` / `Protocol` base, an `ABCMeta` \
metaclass, or an `@abstractmethod`). The distance from the main sequence is `D = |A + I - 1|`. \
Near `A = 0, I = 0` lies the zone of pain (concrete code many modules depend on: every change \
ripples); near `A = 1, I = 1` the zone of uselessness (abstractions nobody uses). Only modules with \
`Ca + Ce` of at least `distance-min-coupling` are judged: for barely coupled modules the ratios \
mean little. The rule is a ranking aid, which is why it is off by default.\n\n\
Fix: in the zone of pain, put an abstract interface between the module and its dependants; in \
the zone of uselessness, remove or merge unused abstractions.",
        severity: Severity::Info,
        default_enabled: false,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Coupling,
        thresholds: &[ThresholdKey::MaxDistance, ThresholdKey::DistanceMinCoupling],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::GOD_MODULE.0,
        name: "god-module",
        summary: "Module that is both large (`max-module-symbols`) and highly coupled (`max-module-coupling`)",
        message: "module `{0}` defines {1} classes and functions (> {2}) and is coupled to {3} modules (> {4})",
        args: &[ArgKind::Symbol, ArgKind::U32, ArgKind::U32, ArgKind::U32, ArgKind::U32],
        explanation: "A module with more classes and functions (methods and nested functions \
included) than `max-module-symbols` whose total coupling `Ca + Ce` (distinct first-party modules \
importing it plus distinct first-party modules it imports, `if TYPE_CHECKING:` imports excluded) \
exceeds `max-module-coupling`. Such a module gathers many responsibilities and sits in the middle \
of the import graph, so most changes touch it and it touches most of the code base.\n\n\
Fix: split the module along its responsibilities so that each dependant imports only the part it \
needs.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Coupling,
        thresholds: &[ThresholdKey::MaxModuleSymbols, ThresholdKey::MaxModuleCoupling],
        confidence: ConfidenceModel::Certain,
    },
    RuleDef {
        code: codes::GOD_CLASS.0,
        name: "god-class",
        summary: "Class that is both complex (`max-class-wmc`) and uses many other classes (`max-class-fan-out`)",
        message: "class `{0}` is a god class (WMC {1} > {2}, uses {3} other classes > {4})",
        args: &[ArgKind::Symbol, ArgKind::U32, ArgKind::U32, ArgKind::U32, ArgKind::U32],
        explanation: "After Lanza and Marinescu's detection strategy. WMC (weighted methods per \
class) is the sum of the McCabe complexities of the class's methods and class-level statements; \
the class fan-out is the number of distinct other classes whose constructors or methods the \
class's methods call (resolved calls of any confidence; calls into the class itself and to plain \
functions are not counted). A class above both `max-class-wmc` and `max-class-fan-out` \
centralises behaviour and reaches into many collaborators instead of delegating to them.\n\n\
Fix: move behaviour next to the data it uses (into the collaborating classes), and split the \
class along groups of methods that share state.",
        severity: Severity::Warning,
        default_enabled: true,
        cost: CostClass::Graph,
        gate: Gate::None,
        pass: Pass::Coupling,
        thresholds: &[ThresholdKey::MaxClassWmc, ThresholdKey::MaxClassFanOut],
        confidence: ConfidenceModel::Certain,
    },
];
