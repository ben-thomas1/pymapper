# PyMapper

PyMapper is a Rust command-line tool for exploring Python code without executing
it. It maps functions, classes, imports and calls, reports complexity hotspots,
and identifies functions with no resolved callers. Its main output is a JSON
analysis bundle whose edges include confidence labels and resolution reasons.

The current `pymapper` CLI provides `map`, `scan` and `explain`. A separate
`pymapper-v1` binary retains the original reports, threshold checks, baselines,
cycle detection and terminal explorer. The two pipelines have different
resolution rules and can produce different results.

## Build and install

Requirements: Rust **1.93 or newer**, Cargo, and a C compiler for tree-sitter and
the syntax highlighter. On macOS, install the Xcode Command Line Tools; on
Debian/Ubuntu, install `build-essential`. Python is not required for analysis.
Graphviz is optional for rendering DOT; Python 3 is needed for the terminal
smoke test. macOS and Linux are the supported build targets; Windows is untested.

From the repository root:

```sh
cargo build --locked --release
./target/release/pymapper --help
cargo install --locked --path . --bin pymapper --bin pymapper-v1
```

Installation places both binaries in Cargo's bin directory. Add that directory
to `PATH` if needed. Use `--locked` to reproduce the checked-in dependencies.
There is no Python package to install and no API key or service to configure.

## Analyze Python

These examples use the small synthetic fixtures included in the repository:

```sh
cargo run --locked -- map tests/fixtures/simple.py
cargo run --locked -- scan tests/fixtures/cross_module --format json
cargo run --locked -- explain greet tests/fixtures/simple.py
cargo run --locked -- map tests/fixtures/inheritance.py --format dot
```

Replace the fixture path with one or more Python files or directories. Directories
are searched recursively for `.py` files. Overlapping inputs are deduplicated.
Discovery skips hidden directories, `__pycache__`, `venv`, `node_modules`,
`target`, `build`, `dist` and `gitbackup`, and does not follow symlinks found
during a walk. An explicitly supplied path is resolved, including symlinks.
Git ignore files are **not** consulted. Supply only directories you intend to
analyze; explicitly named excluded directories can still be analyzed.

For an installed binary:

```sh
pymapper scan tests/fixtures/decorators.py --profile pytest
pymapper map tests/fixtures/simple.py --confidence certain --format json
pymapper explain Greeter tests/fixtures/simple.py --format json
```

| Option | Values | Default |
| --- | --- | --- |
| `--format` | `table`, `json`, `dot` (`explain`: table/json) | `table` |
| `--profile` | `general`, `pytest` | `general` |
| `--confidence` | `certain`, `likely`, `possible`, `unresolved` | `possible`; `scan` uses `unresolved` |

Confidence is an ordered heuristic, not a measured probability. `certain` means
the analyzer matched a declared local/imported target; `likely` includes simple
instance construction; `possible` includes a unique method-name match with an
unknown receiver; `unresolved` includes external and dynamic calls. A threshold
includes that level and all more certain levels. It filters edges and findings
in every output format. Metrics describe the full analyzed graph, including
resolved edges through `possible`, regardless of the display threshold.

`map` and `scan` JSON use schema version 2, with `project`, `modules`, `symbols`,
`imports`, `edges`, `metrics`, `findings` and `diagnostics`. `explain` JSON contains
`matches` and their filtered `edges`. Symbol IDs include the input's resolved
path and byte span: they distinguish repeated names but change when files move
or text is inserted. Reports contain source paths and names; review them before
sharing them.

DOT includes edges between resolved symbols; use JSON or table output to inspect
import records and unresolved targets.

Exit code 0 means analysis completed, including when findings exist. Missing,
unreadable or syntactically invalid inputs and unmatched `explain` queries fail
with a nonzero exit code; invalid CLI values use code 2. There is no v2 CI
severity gate. PyMapper does not rewrite Python source or execute imports.

To render a graph, choose a new output filename (shell redirection and Graphviz
can overwrite existing files):

```sh
pymapper map tests/fixtures/simple.py --format dot | dot -Tsvg -o call-graph.svg
```

## Legacy reports and terminal explorer

```sh
cargo run --locked --bin pymapper-v1 -- report tests/fixtures/complex.py --format json
cargo run --locked --bin pymapper-v1 -- report tests/fixtures/complex.py --sort cognitive --top 5
cargo run --locked --bin pymapper-v1 -- cycles tests/fixtures/cycles.py
cargo run --locked --bin pymapper-v1 -- dead-code tests/fixtures/inheritance.py --classify
cargo run --locked --bin pymapper-v1 -- tui tests/fixtures/simple.py
```

The TUI has tabs `0`–`5`, `j`/`k` navigation, `s` sorting and `Enter` for source.
In the graph tab, `Tab` changes focus, `/` searches, `v` switches local/global
views and `t` opens a call tree. `q` exits or closes an overlay; `Ctrl-C` exits
from any view. The TUI is available only in v1.

Legacy threshold checks accept `cc`, `cognitive`, `nesting`, `loc` and `params`:

```sh
pymapper-v1 report tests/fixtures/simple.py --fail-above cc=10,cognitive=15
pymapper-v1 report tests/fixtures/simple.py --save-baseline baseline.json
pymapper-v1 report tests/fixtures/simple.py --compare baseline.json --fail-regression
```

A saved baseline must have a **new destination path**; existing files and
symlinks are never replaced. Saving writes a temporary file in the destination
directory before publishing the completed JSON. Comparison reports McCabe,
cognitive and LOC changes by qualified function name; duplicate definitions
and renamed functions limit its usefulness. `--compare` emits a text comparison,
so do not combine it with JSON output expecting a single JSON document.

Only v1 reads `.pymapper.toml` or `[tool.pymapper]` in `pyproject.toml`, searching
upward from the current directory. See [.pymapper.toml.template](.pymapper.toml.template).
Malformed configuration is an error. v2 uses command-line options only.

## Tests

```sh
cargo test --locked --all-targets
cargo test --locked --release --all-targets
cargo clippy --locked --all-targets
cargo build --locked --bins
python3 tests/tui_smoke.py target/debug/pymapper-v1
cargo bench --locked --bench benchmarks
```

The test suite covers generated inputs, cross-module resolution, metrics,
failure exits, input deduplication, baseline preservation and CLI output. The
Unix PTY smoke test exercises the real terminal application and checks terminal
restoration. Criterion benchmarks use the included synthetic fixtures; they
are microbenchmarks, not evidence of performance on large repositories.
See [validation notes](docs/validation.md) for checks actually executed.

## Limits

- This is a best-effort static analyzer, not a Python type checker or proof of
  dead code. Review candidates before deleting code. Public APIs, callbacks,
  framework discovery and runtime imports can have no visible callers.
- Module-level calls, lambdas, conditional definitions, wildcard imports,
  re-exports, `global`/`nonlocal`, descriptors, monkey patching and full dataflow
  are not modeled completely. Local assignment inference is flow-insensitive.
  Inheritance traversal does not implement Python's full C3 method resolution.
  Confidence labels do not override these limitations.
- Module names are relative to the common input directory. Analyze a package's
  parent directory when imports use that package name. Python import search
  paths and installed dependencies are not loaded.
- Inputs must be UTF-8. Tree-sitter syntax checks are not equivalent to Python's
  compiler checks. Syntax depth above 256 is rejected to bound recursive walks.
- Complexity and maintainability scores use documented heuristics, without
  empirical calibration against defects or human judgments. `call_depth` is
  maximum shortest-path distance, not runtime stack depth. Cycles are mutually
  reachable groups, not ordered execution traces; v1 omits singleton recursion.
- v1 uses name-based identities and simpler import/inheritance resolution.
  Its grades are exploratory; CSV/Markdown are intended for ordinary identifier
  paths. Prefer JSON for machine processing. `grade` supports table and JSON.
- Whole-project data stays in memory, and reachability metrics can take
  O(V × (V + E)) time. Large repositories can be slow. TUI global layout shows
  at most 500 nodes; call trees stop at 128 levels or 10,000 displayed entries.

See [architecture](docs/architecture.md) and [metric definitions](docs/implementation.md).
