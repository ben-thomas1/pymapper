# pymapper

Whole-program analysis for Python codebases: import cycles, layering contracts,
cross-module dead code, duplication, coupling and complexity, with readable grouped output, an
agent mode (`--llm`), rule codes, config, suppressions, baselines and exit codes. ruff and ty check one file at a time;
pymapper looks at the whole program (and, optionally, the venv and the standard library it
links to).

**Status:** under rewrite. Every subcommand works end to end: `check`, `explain`, the queries
(`callers`, `callees`, `impact`, `path`, `imports`, `cycles`, `summary`), `deps` and `config`.
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
pymapper check                       # the whole project around the working directory
pymapper check src/pkg tests/x.py    # analyse the whole project, report only these paths
pymapper check --llm                 # compact, prioritised report for coding agents
pymapper explain PM401-e3b2          # one finding in depth: evidence, source, effect, how to verify and fix
pymapper check --select PM3,PM4      # only cycles/contracts and dead code
pymapper check --ignore PM1          # everything but complexity
pymapper check --group-by file       # one section per file instead of per category
pymapper check --format json         # human (default) | llm | concise | json | sarif | github
pymapper check --confidence 50       # also show less certain findings (default gate: 80)
pymapper check --deps --stdlib       # link first-party code to the venv and the stdlib
pymapper check --save-baseline .pymapper-baseline.json   # record today's findings
pymapper check --baseline .pymapper-baseline.json        # report only new ones
pymapper check --stats               # phase timings and peak RSS on stderr
pymapper check --watch               # check again after every change (incremental update)
pymapper deps --score                # third-party distributions: use by first-party code, grades (docs/deps.md)
pymapper deps --check                # dependency hygiene: undeclared, unused, private imports (PM6xx)
pymapper config                      # the effective configuration
pymapper config --template           # every option, commented, with its default and cost
```

The default output is for people: findings grouped by category (architecture, dependencies,
complexity, coupling, duplication, dead code, unused imports), then by file, with the metric
findings of one function merged into one line and a stable id at the end of every item
(coloured on a terminal):

```
$ pymapper check
Architecture  2
  models/order.py
     2:29  imports forbidden repo.order_repo (contract: models are independent of repo)   PM303-e6ff
  repo/order_repo.py
     1:26  import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo   PM301-e35f

Complexity  1 function
  services/pricing.py
     6:5   calculate_price  McCabe 14>10 · cognitive 32>15 · nesting 5>4 · params 8>7  PM101-312c +PM102,PM103,PM105

Dead code  18
  app.py
    35:5   function  dead_endpoint          never used                                    PM401-e3b2
  ...
  utils/formatting.py
     5:5   function  format_currency        only used by unused code                      PM401-5fdd
  utils/legacy_helpers.py
     1:1   module    utils.legacy_helpers   never imported                                PM404-c008
  ...

Unused imports  2
  services/pricing.py
     1:8   json                                                                           PM405-4ed8
  ...

26 findings in 15 files (23 files checked) · architecture 2 · complexity 1 function · dead code 18 · unused imports 2
10 more below confidence 80 (use --min-confidence 0 to show them)
```

(The seeded demo project in `tests/corpus/demo`.) The formats, and who each is for:

| format | for | notes |
|--------|-----|-------|
| `human` (default) | people at a terminal | grouped and merged as above; `--group-by file`; `--color auto\|always\|never` (`NO_COLOR` honoured); never capped |
| `llm` (`--llm`) | coding agents | header facts (root, dead-code mode and why, counts, what the exit code means, blind spots), items in priority order with ids, evidence, a fix hint and a verification command; capped at `--limit` items (default 50, `0` = all) with a final `N more findings not displayed` line |
| `concise` | scripts and editors that parse lines | `path:line:col: CODE message`, byte-identical to 0.3.0 |
| `json` | programs | one document, schema version 2, with `root` and a per-finding `id` and `fix` |
| `sarif`, `github` | code scanning, GitHub Actions annotations | |

Every command has an `--llm` mode, and `--llm` conflicts with `--output-format` (exit 2).
Paths are relative to the working directory, except in JSON and SARIF (relative to the project
root). An id such as `PM401-e3b2` stays the same while the code it points at does not change
(it is derived from the baseline key, never from line numbers), so an agent can read
`check --llm`, then run `pymapper explain <id>` before acting on an item. `explain` re-runs the
analysis, so give it the same options as the `check` run; it also takes a symbol
(`pymapper explain services.pricing:calculate_price`). The config keys `output-format`, `group-by`, `color` and
`limit` set the same defaults.

The analysis is always whole-program: positional paths only filter which findings are
reported (`pymapper check src/pkg` gives exactly the findings of the whole-project run that lie
under `src/pkg`, with the same module names), and the footer says so (`Filtered to src/pkg (12 of
340 first-party files); the whole project was analysed.`).

Exit codes (unchanged): **0** clean, **1** findings at or above the confidence gate, **2** usage
error (including a path to check that does not exist, an unknown rule selector such as `PM999`,
or a selection of which no rule can run, e.g. `--select PM6` or `--select PM501` without
`analysis.dupes = true`), invalid configuration or baseline, or a fatal I/O error. Files and
directories that exist but cannot be read are warnings.
Output to a closed pipe (`| head`) stops quietly.

Full reference (pipeline, every flag, output formats, `explain`, baselines, `--stats`):
[`docs/cli.md`](docs/cli.md). A guided tour with real output: [`docs/usage.md`](docs/usage.md).

### Queries

Questions about the code, answered from the same whole-program load as `check` (same root,
config, flags and cache). The text output is one fact per line, at the `path:line:col` it is
about (a call site, an import statement; relative to the working directory), call edges tagged
with their confidence, then one summary line. `--llm` groups the rows by file with paths and
symbols once, states the blind spots of the query (callbacks passed as values, getattr dispatch
and C callers are not call edges) and ends with a next step; `--format json` gives one document
with `check`'s conventions. `--limit N` caps the rows (default 200, 50 with `--llm`; `0` = all).

```
pymapper callers app.util:helper             # who calls it (--depth N, --transitive)
pymapper callees Engine.run                  # what it calls; a last-segments suffix is enough
pymapper impact app.models:User --depth 3    # everything that transitively calls it, tests, importers
pymapper path app.cli:main app.db:connect    # a shortest call chain (--imports: import chain)
pymapper imports app.billing --depth 1       # module imports in and out, rolled up to packages
pymapper cycles                              # every import cycle (--symbol X [--calls])
pymapper summary                             # sizes, resolution rates, cycles, top hotspots
```

```
$ pymapper callers services.user_service:create_user
app.py:14:13: app:list_users [certain]
app.py:26:5: app:main [certain]
middleware/pipeline.py:17:12: middleware.pipeline:run_pipeline [certain]
3 callers of services.user_service:create_user (function, services/user_service.py:9)
```

A symbol is a qualified id (`pkg.mod:Class.method`), a dotted name (`pkg.mod.Class.method`) or
its last segments (`Class.method`, `method`). An ambiguous symbol lists its candidates on stdout
and exits 2; an unknown one lists similar names and exits 2. Positional paths filter the rows
(`pymapper callers helper src/api`), never what is loaded; `path` takes `--root DIR` to query
a project from outside it. Details:
[`docs/cli.md`](docs/cli.md#query-subcommands).

## Configuration

`[tool.pymapper]` in `pyproject.toml`, or a standalone `pymapper.toml` (top-level keys, no
`[tool.pymapper]` header). Discovery walks up from the first path (else the working directory);
the nearest config file applies. Precedence: defaults < config file < `--set 'KEY = VALUE'` <
typed flags. Validation is strict (unknown keys are errors with file:line:col).

The project root does not depend on how deep the first path is. It is, in order: the directory
of the config file found (`--config` included; not with `--isolated`); else, walking up from the
first path, the outermost directory whose `pyproject.toml` has `[tool.uv.workspace]`; else the
top of the git repository (the nearest `.git` directory or file; the walk never crosses it);
else the outermost directory with a `pyproject.toml` or `pymapper.toml`; else the first path
itself (its directory, for a file). `$HOME` and its parents are never chosen for a path below
`$HOME`. Every directory below the root with a `pyproject.toml` is a project unit (dead-code
mode per unit) and an import root (its build-backend source dirs, its `src/` layout, then the
directory itself), so uv workspace members and monorepo packages get the right module names
without a venv; a `pyproject.toml` inside a package directory (one with `__init__.py`) is
neither. The venv is looked up in the project root (`.venv` / `venv`) unless `venv` or
`$VIRTUAL_ENV` says otherwise.

```toml
[tool.pymapper]
paths = ["src", "tests"]
extend-exclude = ["migrations"]
mode = "application"                 # dead code: auto | application | library
presets = ["pytest", "fastapi"]      # framework entrypoints
min-confidence = 80
baseline = ".pymapper-baseline.json"
output-format = "human"              # human | llm | concise | json | sarif | github
group-by = "category"                # human output: category | file
color = "auto"                       # human output: auto | always | never (NO_COLOR honoured)
limit = 50                           # llm output: items shown (0 = all)

[tool.pymapper.rules]
ignore = ["PM107"]
per-file-ignores = { "tests/" = ["PM4"] }

[tool.pymapper.thresholds]
max-mccabe = 12

[tool.pymapper.analysis]
dupes = true                         # expensive: duplicate-code detection (PM501)
include-tests-in-metrics = false     # PM1xx / PM2xx / PM5xx skip test code by default

[[tool.pymapper.contracts]]
name = "architecture"
kind = "layers"
layers = ["app.api", "app.services", "app.db"]
```

Every option, its default and its cost: `pymapper config --template`
([`pymapper.toml.template`](pymapper.toml.template)).

### Choosing a dead-code mode

- `library`: public (non-underscore) names of public modules are API, hence live; only private
  or unreachable internals are reported.
- `application`: only what entry points reach is live: `__main__` modules and guards,
  `[project.scripts]` / entry points, tests, `[entrypoints]` rules and presets.
- `auto` (default): decided per package. Every directory with a `pyproject.toml` (the root and
  nested monorepo packages) is a unit, and each file belongs to its nearest unit. A unit with
  entry points of its own (`[project.scripts]` in its `pyproject.toml`, a plausible
  `__main__` module or guard) is an application, any other a library. `__main__` guards in
  tests, private modules (`_doctools.py`) and `scripts/`, `tools/`, `examples/`, `docs/`,
  `benchmarks/` do not count (they are still roots). `check --stats` prints
  `deadcode.units`, `deadcode.library`, `deadcode.application` and one `deadcode.mode` line
  per unit with the deciding evidence, e.g.
  `deadcode.mode apps/x: application ([project.scripts] in apps/x/pyproject.toml)`.

Force one mode for every unit with `--mode application|library` or `mode = "..."`. An
application-mode unit with no entry point at all gets a warning, since everything in it would
look unused. Functions a framework loads by name from non-Python configuration (YAML tool
lists, plugin registries) are found through config-file evidence (`analysis.config-evidence`,
on by default): `*.yaml`, `*.yml`, `*.toml`, `*.json`, `*.cfg` and `*.ini` files of the
first-party walk (gitignore and excludes apply; files over 1 MiB, lockfiles, pymapper /
coverage.py JSON reports and the configured `analysis.coverage` report are skipped) are
tokenized once and cached. A full dotted path
(`pkg.mod.func`, `pkg.mod:Class`) keeps a definition or module; a bare name lowers the
confidence to 50 (10 when the file also names the module). Cost: one read per config file on a
cold cache (a few ms for hundreds of files), a stat per file when warm. Rules you know better
can still be declared:

```toml
[tool.pymapper.entrypoints]
names = ["aiml_ais_mcp.elnb.tools:*"]   # every definition of that module is an entry point
# names = ["aiml_ais_mcp.elnb.tools"]   # the same: a dotted name without `:` means `module:*`
```

A user entrypoint pattern (`decorators`, `bases`, `members`, `names`) that matches nothing in the project
gets one warning after the dead-code pass, so a typo does not silently keep code "dead".

Inline suppression: `# pymapper: ignore[PM401]` (or `ignore[PM4]`, or a bare
`# pymapper: ignore`) on the reported line. Ruff's `# noqa` / `# noqa: F401` on an import line
also suppresses PM405 (unused import); other `noqa` codes are ignored. Malformed codes
(`ignore[E501]`, `ignore[pm405]`), unknown codes (`PM999`) and empty lists (`ignore[]`) get a
`path:line` warning and suppress nothing.

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

Metric rules (PM1xx, PM2xx) and duplication (PM5xx) skip test code by default (`tests/`, `test/`,
`testing/`, `_tests/`, `_testing/` directories, `test_*.py`, `*_test.py`, `conftest.py`); set
`analysis.include-tests-in-metrics = true` to include it. Dead code and cycles always cover
tests. Function and module lengths (PM104, PM107) do not count docstrings. JSON output carries
structured fields (`id`, `fix`, `end_line`, `value` / `threshold`, the cycle's members, the other
copy of a duplicate): see [`docs/cli.md`](docs/cli.md#output-formats).

Dead-code findings carry a confidence (0-100): 100 means nothing references the symbol by any
route pymapper knows; lower values mean only weak evidence (a matching attribute name or
string elsewhere, dynamic dispatch) keeps it possibly alive. `check` reports findings at or
above `min-confidence`. Each finding names its evidence: the message says "is never used"
only when nothing refers to the symbol, and the reasons are listed, e.g.
`(confidence 50: name-elsewhere)` in text and `"reasons": ["name-elsewhere"]` in JSON. Every
first-party Python file counts as evidence, importable or not; a symbol mentioned only by tests
is test-only API, reported below the gate as `used-only-in-tests`.

Runtime evidence from a test run sharpens the scores: pass a coverage.py JSON report with
`pymapper check --coverage coverage.json` (or `analysis.coverage`). Code that ran is never
reported; code that never ran and has only weak static evidence of use moves above the gate
(`(confidence 84: name-elsewhere, never-executed)`). Create the report with
`coverage run -m pytest && coverage json`.

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
docs/cli.md            `pymapper check`, `explain` and query subcommand reference
docs/usage.md          guided tour with real output
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
