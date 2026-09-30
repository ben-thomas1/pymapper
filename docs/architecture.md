# Architecture

PyMapper ships two Rust binaries and one library. Both parse Python with
 tree-sitter; neither imports or executes the analyzed code.

## Current pipeline

`src/main.rs` validates CLI options and calls `v2::analyze_paths`.
`loader.rs` discovers regular Python files and normalizes overlapping inputs.
`v2.rs` extracts span-identified symbols, imports with scope information, calls
and simple assignments. Resolution produces edges with an explicit kind,
confidence and reason. A graph pass computes fan-in, fan-out, reachability and
shortest-path depth; findings and table/JSON/DOT renderers consume that bundle.
Missing inputs and parse failures stop analysis, so partial results are not
reported as a successful complete scan.

Resolution is lexical where supported, but is not a Python interpreter or a
type-inference engine. See the README's limitations before interpreting edges.
The v2 pipeline is sequential; the legacy loader uses Rayon for parallel parsing.

## Legacy pipeline

`src/v1_main.rs` retains the original commands. `parser.rs` extracts
`types.rs` records, `graph.rs` builds a petgraph directed call graph, and
`metrics.rs`, `report.rs`, `dead_code.rs`, `cycles.rs` and `grade.rs`
provide analyses. v1 identifies functions by module/class/name, so repeated
names can collapse. `complexity.rs` is shared by both pipelines.

`tui.rs` runs legacy loading and graph analysis on a worker thread, propagates
errors to the main event loop, and restores terminal modes with an RAII guard.
`layout.rs` provides layered local layouts and a bounded global force layout.
The terminal explorer does not display v2 confidence labels.

## Files and outputs

Analysis reads source only. The sole application file writer is the legacy
baseline saver, which publishes a completed temporary JSON file without
replacing an existing destination. Shell redirections and Graphviz output are
controlled separately by the caller. Reports include analyzed paths and names.

Dependencies are bounded to compatible release series and recorded in
`Cargo.lock`. Use `--locked` for builds, installs, tests and benchmarks.
