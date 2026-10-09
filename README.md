# pymapper

Whole-program analysis for Python codebases: import cycles, layering contracts,
cross-module dead code, duplication, coupling and complexity, with ruff-style output, rule
codes, config, suppressions, baselines and exit codes. ruff and ty check one file at a time;
pymapper looks at the whole program (and, optionally, the venv and the standard library it
links to).

**Status:** under rewrite. `pymapper check` works end to end; the query subcommands
(`callers`, `callees`, `impact`, `path`, `deps`, `cycles`, `summary`) are not implemented yet.
See [`docs/design.md`](docs/design.md) for the design.

## Install

pymapper is a single self-contained binary with no Python dependency. With a Rust toolchain (the
version is pinned in `rust-toolchain.toml`):

```
cargo install --path crates/pymapper-cli     # installs `pymapper` into ~/.cargo/bin
# or
cargo build --release                         # target/release/pymapper
```

## Usage

```
pymapper check                       # the project around the working directory
pymapper check src tests             # explicit paths (config discovery starts at the first)
pymapper check --select PM3,PM4      # only cycles/contracts and dead code
pymapper check --ignore PM1          # everything but complexity
pymapper check --format json         # concise | json | sarif | github
pymapper check --confidence 50       # also show less certain findings (default gate: 80)
pymapper check --deps --stdlib       # link first-party code to the venv and the stdlib
pymapper check --save-baseline .pymapper-baseline.json   # record today's findings
pymapper check --baseline .pymapper-baseline.json        # report only new ones
pymapper check --stats               # phase timings and peak RSS on stderr
pymapper config                      # the effective configuration
pymapper config --template           # every option, commented, with its default and cost
```

```
$ pymapper check
app/billing.py:1:1: PM301 import cycle: app.billing -> app.accounts -> app.billing
app/report.py:42:5: PM101 `app.report:render` is too complex (McCabe 14 > 10)
app/legacy.py:10:5: PM401 function `app.legacy:old_export` is never used
Found 3 findings in 3 files (128 files checked).
```

Exit codes: **0** clean, **1** findings at or above the confidence gate, **2** usage error
(including a path to check that does not exist), invalid configuration or baseline, or a fatal
I/O error. Files and directories that exist but cannot be read are warnings.
Output to a closed pipe (`| head`) stops quietly.

Full reference (pipeline, every flag, output formats, baselines, `--stats`):
[`docs/cli.md`](docs/cli.md).

## Configuration

`[tool.pymapper]` in `pyproject.toml`, or a standalone `pymapper.toml` (top-level keys, no
`[tool.pymapper]` header). Discovery walks up from the first path; the directory holding the
config file is the project root. Precedence: defaults < config file < `--set 'KEY = VALUE'` <
typed flags. Validation is strict (unknown keys are errors with file:line:col).

```toml
[tool.pymapper]
paths = ["src", "tests"]
extend-exclude = ["migrations"]
mode = "application"                 # dead code: auto | application | library
presets = ["pytest", "fastapi"]      # framework entrypoints
min-confidence = 80
baseline = ".pymapper-baseline.json"

[tool.pymapper.rules]
ignore = ["PM107"]
per-file-ignores = { "tests/" = ["PM4"] }

[tool.pymapper.thresholds]
max-mccabe = 12

[tool.pymapper.analysis]
dupes = true                         # expensive: duplicate-code detection (PM501)

[[tool.pymapper.contracts]]
name = "architecture"
kind = "layers"
layers = ["app.api", "app.services", "app.db"]
```

Every option, its default and its cost: `pymapper config --template`
([`pymapper.toml.template`](pymapper.toml.template)).

Inline suppression: `# pymapper: ignore[PM401]` (or `ignore[PM4]`, or a bare
`# pymapper: ignore`) on the reported line.

## Rules

| range | category | rules |
|-------|----------|-------|
| PM1xx | complexity and size | McCabe (exactly ruff C901), cognitive complexity, nesting, function / module length, parameters, maintainability index |
| PM2xx | coupling | fan-out, distance from the main sequence, god modules, god classes |
| PM3xx | import cycles and layering | import cycles, layer / forbidden / independence contracts |
| PM4xx | dead code | unused functions, classes, methods, modules, imports (whole-program reachability with a confidence score) |
| PM5xx | duplication | duplicated code across files (gated: `analysis.dupes`) |
| PM6xx | dependencies | undeclared / unused / private / transitive dependencies (not run by `check` yet) |

Each rule's explanation, thresholds, cost class and confidence model:
[`docs/rules.md`](docs/rules.md).

Dead-code findings carry a confidence (0-100): 100 means nothing references the symbol by any
route pymapper knows; lower values mean only weak evidence (a matching attribute name or
string elsewhere, dynamic dispatch) keeps it possibly alive. `check` reports findings at or
above `min-confidence`.

## Performance

Performance (time and memory) is a first-class requirement: the target is millions of lines.
Sources are streamed (read, parse, drop), names are interned, tables are struct-of-arrays, and
every analysis declares a cost class; expensive ones are gated behind config. Measured on a
14-core Apple Silicon Mac, release build, warm OS file cache (details and more numbers in
[`docs/benchmarks.md`](docs/benchmarks.md#check-s45)):

| `pymapper check` on | files | LOC | cold cache | warm cache | peak RSS |
|---------------------|-------|-----|------------|------------|----------|
| CPython 3.13 stdlib (as a project) | 630 | 305k | 54 ms | 19 ms | 22-28 MB |
| a 478k-LOC application | 1,803 | 478k | 161 ms | 102 ms | 42-49 MB |
| the same with `--deps --stdlib` | 63,017 | 17.8M | 2.75 s | 0.52 s | 0.68-0.70 GB |
| a 60k-file venv as first-party code | 60,529 | 17.0M | 3.4 s | 1.5 s | 0.70-0.75 GB |

- The first-party walk is the floor of a warm run on huge first-party trees (0.86 s of the
  1.5 s above); third-party code (`--deps`) comes from `RECORD` file lists and is not walked.
- The cache (`.pymapper_cache`, flat mmap segments) stores first-party facts per file
  (validated by size + mtime, then content hash), third-party per distribution and the stdlib
  per interpreter. `--no-cache` disables it.
- `--threads N` bounds the worker pool; peak memory grows slowly with threads.
- `analysis.dupes` costs a token pass during extraction plus matching; see the template for
  measured numbers.

## Layout

```
crates/pymapper-core   library (all analysis)
crates/pymapper-cli    the `pymapper` binary
crates/pymapper-gui    the `pymapper-gui` binary (native GUI; see docs/gui.md)
xtask                  dev tooling: cargo xtask bench | diff-ruff | fuzz
tests/corpus           Python inputs used by tests and benchmarks
docs/design.md         design document (source of truth)
docs/cli.md            `pymapper check` reference
docs/rules.md          rule reference (generated)
```

## Development

```
cargo build --release          # target/release/pymapper
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --profile profiling   # release + line tables, for sampling profilers
```

Benchmarks (time per phase and peak RSS): `cargo xtask bench`, see
[`docs/benchmarks.md`](docs/benchmarks.md). Fuzzing (stable-Rust mutation fuzzer for the
lexer/parser): `cargo xtask fuzz`, see [`docs/fuzz.md`](docs/fuzz.md). GUI (not a default
member): `cargo build --release -p pymapper-gui`, see [`docs/gui.md`](docs/gui.md).

## Licence

MIT, see [`LICENSE`](LICENSE).
