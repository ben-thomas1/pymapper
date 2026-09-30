# Analysis definitions

These scores are descriptive heuristics. They are not validated defect
predictors, runtime profiles, or calibrated probabilities.

## Per-function metrics

- **McCabe (CC):** one plus syntactic decisions: if/elif, for/while, except,
  with, assert, conditional expressions, comprehension filters and boolean
  operators. This is a project-specific variant, not exact Python control flow.
- **Cognitive:** control constructs add 1 plus their nesting level; else/elif
  and boolean operators add 1. Nested function/class/lambda bodies are excluded.
- **Nesting:** deepest supported control-structure nesting inside the function.
- **LOC:** inclusive source span, including the definition line and any nested
  definitions, blank lines or comments inside that span.
- **Halstead volume:** N × log2(vocabulary), using lexical operators and operands
  in the function body. Strings count as operands; this is a syntax heuristic.
- **Maintainability index:** clamp((171 - 5.2 ln(V) - 0.23 CC - 16.2 ln(LOC)) ×
  100 / 171, 0, 100), with V replaced by 1 when zero. Class records in v2 carry
  placeholder complexity/MI fields; interpret these metrics on functions.

Nested executable scopes are excluded from body metrics, but Python features
such as match/case and comprehensions are not modeled as a full control-flow
 graph. Values should only be compared using the same PyMapper version.

## Graph metrics and findings

Fan-in/out count distinct resolved neighbors. Reachability includes the starting
symbol. Depth is the largest shortest-path distance to a reachable symbol,
computed by breadth-first traversal; it stays finite in recursive graphs.
v2 includes call/decorator edges through `possible` confidence and pytest
entrypoint edges. Display confidence filtering does not recompute metrics.
Legacy transitive LOC/CC sum each reachable internal function once.

v2 reports hotspots at CC >= 10, cognitive >= 15 or nesting >= 5; higher CC or
cognitive values increase severity. Coupling findings start at fan-in + fan-out
>= 10. Dead-code candidates have no resolved callers and exclude known
entrypoints, dunder names and `main`. The pytest profile recognizes test names
and fixture decorators; this approximates pytest collection and does not run it.
Unresolved calls include ordinary builtins and external dependencies.

Legacy module coupling counts distinct dependencies on other analyzed modules,
using resolved calls and import records. Cross-module call counts include only
resolved calls into a different analyzed module. v1 module summaries omit
modules with no extracted functions. Cycles are strongly connected components
with at least two nodes; listing order does not imply a cycle path.

## Legacy grades and configuration

Grades average heuristic per-function scores for complexity, cognitive load,
maintainability, dead-code candidates, nesting and size, plus module coupling.
Weights and cutoffs are in `src/grade.rs`. There is no benchmark establishing
that a letter grade measures software quality. Empty JSON grades use null
score/grade fields rather than suggesting that no code earns a perfect score.

Thresholds accept nonnegative integers for `cc`, `cognitive`, `nesting`,
`loc` and `params`; unknown, malformed and duplicate keys are errors.
Thresholds apply to the full analysis even when a report is filtered or displayed
as a histogram. Baselines compare CC, cognitive and LOC by qualified name.
Neither thresholds nor baselines execute Python.
