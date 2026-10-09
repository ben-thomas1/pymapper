# `pymapper check`

`pymapper check [PATHS]...` loads a project, runs every analysis that has an enabled rule and
reports findings, ruff-style. It is meant to run in CI and in agent loops next to pytest, ruff
and ty.

The analysis is always whole-program. `PATHS` (files or directories) only filter the report: a
finding is reported when its file lies under one of them (component-wise, lexically; symlinks
are not followed). `pymapper check src/pkg` therefore prints exactly the findings of the
whole-project run that lie under `src/pkg`, with the same module names and confidences. Without
`PATHS` everything is reported.

## Pipeline

```
pymapper check [paths]
  │
  ├─ config        pyproject.toml [tool.pymapper] / pymapper.toml, --set, flags     phase config
  ├─ roots         import roots: src layout, venv, stdlib, .pth, dist-info          phase roots
  ├─ walk          first-party walk (gitignore + exclude), stdlib walk,             phase walk
  │                third-party RECORD file lists (no walk of distributions)
  ├─ index         read -> lex -> parse -> facts -> drop source, through the cache  load.*, cache.write
  ├─ imports       absolute/relative, re-exports, __all__, star, TYPE_CHECKING      resolve.modules/imports/edges
  ├─ analyses      only those with an enabled rule
  │    ├─ hotspot      PM101-PM107  per-function / per-module metrics          O(n)
  │    ├─ cycles       PM301        SCCs of the first-party import graph       O(V+E)
  │    ├─ layers       PM302-PM304  import contracts (only with [[contracts]]) O(E) per contract
  │    ├─ calls        call resolution, only for dead code / PM204             O(n)
  │    ├─ coupling     PM201-PM204  (call graph only when PM204 is enabled)     O(V+E)
  │    ├─ dead code    PM401-PM405  reachability + name evidence               O(V+E)
  │    └─ dupes        PM501        token clones, gated by analysis.dupes      expensive
  ├─ scope         keep findings under the positional PATHS (if any)
  ├─ filter        selection, per-file-ignores, # pymapper: ignore[...], confidence gate
  ├─ report        deterministic order (path, line, col, rule, symbol)
  ├─ baseline      drop findings recorded in --baseline / `baseline`
  └─ output        concise | json | sarif | github, then the exit code
```

Third-party (`--deps`) and stdlib (`--stdlib`) code is loaded so first-party code links to it
(imports, base classes, calls), but it is never reported by `check` and never affects the exit
code. Only first-party `.py` files are analysed for findings: importable modules and files that
cannot be imported (tests in hyphenated directories such as `tests/my-service/test_x.py`,
scripts such as `scripts/bump-version.py`; never reported as unused modules). Stubs and files
shadowed by another file of the same module are not analysed, but every first-party `.py` file
contributes dead-code name evidence; a name used only by test code gets the reason
`used-only-in-tests`.

The PM6xx dependency-hygiene rules are run by `pymapper deps --check`, not `check` (see
"Rules that cannot run" below and [`deps.md`](deps.md#hygiene---check)).

`pymapper deps` (third-party distributions, their use by first-party code, `--score`
scorecards and the `--check` dependency-hygiene rules) is described in [`deps.md`](deps.md).
The query subcommands (`callers`, `callees`, `impact`, `path`, `imports`, `cycles`, `summary`)
are described in [Query subcommands](#query-subcommands) below.

## Project root

Config discovery walks up from the first path (default: the working directory); in each
directory `pymapper.toml` wins over a `pyproject.toml` with `[tool.pymapper]`. The project root
is then, in order of precedence:

1. the directory of the config file applied (found, or given with `--config`; `--isolated`
   skips this step);
2. walking up from the first path: the outermost directory whose `pyproject.toml` has a
   `[tool.uv.workspace]` table;
3. the top of the git repository: the nearest ancestor holding `.git` (a directory, or the file
   of a worktree / submodule); the walk never goes past it;
4. the outermost directory with a `pyproject.toml` or `pymapper.toml`;
5. the first path itself (its directory, for a file).

Steps 2-4 never consider `$HOME` or its parents when the first path is below `$HOME`. The root
does not depend on how deep the first path is, so `check src/pkg/mod.py` and `check` agree on the
root, the module names, the venv (`.venv` / `venv` in the root unless `venv` or `$VIRTUAL_ENV`
is set) and the cache. Module names come from the import roots of the whole project: `src`
config, build-backend source dirs and `src/` layouts of the root and of every project unit (a
directory below the root with a `pyproject.toml`, e.g. a uv workspace member), editable installs
in the venv, then the root itself. A `pyproject.toml` inside a package directory (with
`__init__.py`, as pandas ships in its wheel) is not a unit root and adds no import root.

What is loaded is the configured `paths` (default: the project root), plus positional paths
inside the root that `paths` does not cover. A positional path outside the project root is not
checked (warning); run pymapper on that project separately.

## Exit codes

| code | meaning |
|------|---------|
| 0 | no finding at or above the confidence gate (or `--save-baseline` succeeded) |
| 1 | at least one finding at or above the gate |
| 2 | usage error (including a path to check that does not exist, an unknown rule selector, or a selection of which no rule can run), invalid configuration, unreadable or invalid baseline, cannot write output |

A path that does not exist, whether given on the command line or listed in the configured
`paths`, is a usage error (`pymapper: error: path `X` does not exist`, exit 2): a mistyped or
stale path never passes as a clean run. Files and directories that exist but cannot be read,
a missing stdlib or venv, and a cache that cannot be written are warnings on stderr, never
fatal. A closed stdout (`pymapper check | head`) stops output
quietly; the exit code still reflects the findings.

## Flags

Positional `PATHS` filter the report (see above); config and root discovery start at the
first one (default: the working directory). Every configuration flag is shared with the other
subcommands (`pymapper check --help` lists them all):

| flag | config key | notes |
|------|------------|-------|
| `--select`, `--extend-select`, `--ignore` | `rules.*` | comma-separated selectors: `ALL`, `PM1`, `PM10`, `PM101` |
| `--format` / `--output-format` | `output-format` | `concise` (default), `json`, `sarif`, `github` |
| `--confidence` / `--min-confidence` | `min-confidence` | 0-100, default 80; lower shows less certain findings |
| `--deps` / `--no-deps` | `include-deps` | load the venv's third-party packages |
| `--stdlib` / `--no-stdlib` | `include-stdlib` | load the interpreter's standard library |
| `--venv DIR` | `venv` | default `$VIRTUAL_ENV`, then `.venv` / `venv` |
| `--no-cache`, `--cache-dir DIR` | `cache`, `cache-dir` | default `.pymapper_cache` in the project root (see below) |
| `--threads N` / `-j N` | `threads` | 0 = one per logical CPU |
| `--mode` | `mode` | dead code: `auto` (per `pyproject.toml` unit), `application`, `library` (every unit) |
| `--coverage FILE` / `--no-coverage` | `analysis.coverage` | coverage.py JSON report (`coverage json`) as dead-code evidence (see below); accepted by every command, read only by `check` |
| `--preset` | `presets` | flask, fastapi, django, click, pytest, pydantic |
| `--exclude GLOB` | `extend-exclude` | gitignore syntax, relative to the project root |
| `--set 'KEY = VALUE'` | any | TOML fragment, e.g. `--set 'analysis.dupes = true'` |
| `--config FILE`, `--isolated` | | explicit config file / ignore config files |
| `--baseline FILE` | `baseline` | report only findings not in the baseline |
| `--save-baseline FILE` | | record every reported finding, write no report, exit 0 |
| `--stats` | | phase timings, counters and peak RSS on stderr |
| `--watch` | | keep running and check again after every change (see [Watch mode](#watch-mode)) |

A selector that matches no known rule (`--select PM999`, or in `rules.select`, `extend-select`,
`ignore` or `per-file-ignores`) is a usage error (exit 2) with a "did you mean" suggestion when
one is close (`PM409` -> `PM401`). A user entrypoint pattern (`entrypoints.decorators`, `bases`, `members`,
`names`) that matches nothing in the project prints one warning per pattern after the dead-code
pass; a `names` pattern with a `.` but no `:` (`pkg.cli`) names a module and means `pkg.cli:*`
plus the module itself. Malformed inline suppressions (`ignore[E501]`, `ignore[pm405]`,
`ignore[]`, `ignore[PM999]`) print `pymapper: warning: path:line: ...` for files in the reported
paths and suppress nothing; they never change the exit code. An application-mode unit (a directory with a
`pyproject.toml`, see "Choosing a dead-code mode" in the README) without any entry point
(`__main__` guard, scripts, entrypoint rules, tests) prints one warning per unit, or one for
the project when `--mode application` is forced and no unit has one.

With a coverage report (`--coverage FILE` or `analysis.coverage`), code the report shows
executed is live (never reported, nor what it calls), and the remaining doubt of code below
confidence 100 that never ran is divided by 3, with the reason `never-executed` (`name-elsewhere`
50 becomes 84). Statically reachable code that never ran (untested) is not affected. Report
paths are matched to files absolute, relative to the report's directory or the project root,
or by their longest unique path suffix (a report made in another checkout, such as CI). A
report that cannot be read or is not a coverage.py JSON report is an error (exit 2), checked
before the project is loaded when it is missing; one that matches no analysed file, or has line
numbers past the end of a file (stale), prints a warning. The report is read only when a PM4xx
rule runs. The report is never config-file evidence (`analysis.config-evidence`), even inside
the project tree, where `coverage json` writes it by default: it names every function, run or
not. `--stats` adds `coverage_files`, `coverage_matched`, `coverage_executed`,
`coverage_not_executed`, `deadcode.executed` (seeds) and `deadcode.lowered`.

The default cache directory is `.pymapper_cache` in the project root (see "Project root").
So `pymapper check /some/dir` with no config and no marker above it writes
`/some/dir/.pymapper_cache`, even from another working directory (pointing it at an interpreter's stdlib would write into
the Python installation). Use `--cache-dir DIR` or `--no-cache` for directories you do not
own; a cache that cannot be written is only a warning.

## Rules that cannot run

A selected rule does not run in `check` when its `[analysis]` toggle is off (PM501 needs
`analysis.dupes = true`), when it is a dependency rule (PM6xx, run by `pymapper deps --check`),
or when it is a contract rule (PM302-PM304) and no `[[contracts]]` are configured.

- If none of the selected rules can run (`--select PM6`, `--select PM601`, `--select PM501`
  without the toggle, `--select PM302` without contracts), `check` exits 2 and lists the rules
  with the reason and how to enable them. Selecting nothing at all (`--select PM1 --ignore PM1`)
  is an error too.
- If some can run, skipped rules selected by code or category (`PM501`, `PM5`, `PM6`; contract
  rules only by exact code) get a warning, and whenever rules were selected explicitly
  (`select` / `extend-select`, `ALL` included) the footer says how many ran and which were
  skipped:

  ```
  $ pymapper check --select ALL
  All checks passed! (128 files checked)
  17 rules ran, 8 skipped (PM302-PM304: no [[contracts]]; PM501: needs `analysis.dupes = true`; PM601-PM604: run by `pymapper deps --check`).
  ```

- The default selection skips these rules silently (that is what the defaults mean); `--stats`
  always prints `rules_run`, `rules_skipped` and one `skipped ...` line per reason.

## Output formats

- `concise`: `path:line:col: CODE message`, confidence appended when below 100, then a footer
  (`Found N findings in M files (K files checked).`, counts of suppressed / baselined /
  below-gate findings). With positional paths (other than one covering the project root, such
  as `check .` at the root), `K` counts the first-party files under them and
  a line `Filtered to PATHS (K of T first-party files); the whole project was analysed.`
  follows; with an explicit selection that skipped rules, the rules line above follows. Paths
  are relative to the project root; columns are 1-based bytes. Below 100 the confidence comes
  with its reasons for dead-code rules: `(confidence 50: only-used-by-unused-code,
  name-elsewhere)`. The `github` format prints the same footer lines; `json` and `sarif`
  contain the filtered findings only (`files_checked` is `K`).
- `json`: one document (`version` 2, `tool`, `min_confidence`, `findings[]`, `summary`); every
  finding has `code`, `name`, `severity`, `confidence`, `path`, `line`, `col`, `end_line` (last
  line of the reported span: the definition, the whole file for module metrics, the copy for
  PM501, else `line`), `symbol` (the stable qualified id `pkg.mod:Class.method`; for PM405
  `pkg.mod:<bound name>`) and `message`. Rule-specific fields:
  - PM1xx, PM2xx: `value` and `threshold` (the measured value and its limit; for PM203 / PM204,
    which check two limits, the first pair of the message).
  - PM301: `"cycle": {"size": N, "members": [...], "witness": [...]}`: the number of modules in
    the strongly connected component, all of them, and the cycle of the message (without
    repeating its first module).
  - PM501: `"other": {"path", "line", "end_line", "symbol"}`: the other copy of the pair;
    `symbol` (of either copy) is the innermost class or function containing the whole copy,
    else `null`.
  - Dead-code rules (PM4xx): `reasons`, the machine-readable reasons for the confidence
    (`["only-used-by-unused-code", "name-elsewhere"]`); absent when the finding has none.

  Fields are only ever added within a version; version 2 changed the PM405 `symbol` (was the
  module), removed the `#N` suffix from overload implementations (see below) and made PM104 /
  PM107 values exclude docstrings and PM1 / PM2 / PM5 skip test code by default
  (`analysis.include-tests-in-metrics`).
- `sarif`: SARIF 2.1.0 with rule metadata, for code-scanning uploads.
- `github`: GitHub Actions workflow commands (`::warning file=...,line=...::message`).

## Suppressions, per-file ignores, baselines

- Inline: `# pymapper: ignore[PM401]` (or `ignore[PM4]`, or bare `# pymapper: ignore`) on the
  line a finding is reported at. A bare `# noqa` or `# noqa: F401` (ruff's unused import)
  suppresses PM405 on its line, or on every name of a multi-line import from the statement's
  first line; other `noqa` codes suppress nothing.
- Per file: `[rules.per-file-ignores]` maps gitignore-style globs (relative to the project
  root) to selectors: `"tests/" = ["PM4"]`. A pattern matching a parent directory covers every
  file below it.
- Baselines: `pymapper check --save-baseline .pymapper-baseline.json` records today's findings
  by rule + stable symbol id + stable message arguments (never line numbers or metric values),
  so editing a function does not resurface its finding. Then `--baseline FILE` (or
  `baseline = "FILE"` in the config) reports only new findings. The file is JSON, sorted,
  and written atomically; commit it.
- Symbol ids: same-named siblings (a property and its setter, conditional definitions) get a
  `#N` suffix in source order (`name`, `name#2`, ...). In a group with `@overload` signatures
  the implementation (the last non-overload definition) is the plain `name` and the signatures
  are `name#2`, `name#3`, ..., so adding an overload does not change the implementation's id.
  Baselines saved by earlier versions still load; an entry recorded under an old id
  (`Frame.reset_index#4`) no longer matches and its finding is reported as new once: re-save
  the baseline. PM405 baseline entries keep the module id with the name as detail. PM501
  findings inside one definition now key on that definition instead of the file path, so
  baselined duplicates resurface once too.

## `--stats`

```
$ pymapper check --stats
pymapper: stats
  config                      0.5 ms
  roots                       0.9 ms
  walk                        7.9 ms
  load.extract               32.6 ms
  ...
  total                      61.4 ms
  files                       630
  first_party_files           630
  files_checked               630
  ...
  rules_run                    17
  loc                      304805
  ...
  deadcode.units                1
  deadcode.library              1
  deadcode.application          0
  cache_hits                    0
  cache_misses                630
  peak_rss                   24.0 MB
```

Phases are wall time on the coordinating thread, in first-recorded order, with sub-phases
recorded by the core library (`load.*`, `resolve.*`, `deadcode.*`, `dupes.*`). `peak_rss` is
`getrusage` `ru_maxrss` of the whole process. `first_party_files` counts every first-party file
analysed, `files_checked` those under the positional paths; `rules_run` / `rules_skipped` count
the selected rules that ran / could not run, followed by one `skipped CODES: reason` line per
reason. `deadcode.units` counts the project units with
analysed files, split into the library / application modes they resolved to. Measured numbers are in
[`benchmarks.md`](benchmarks.md#check-s45).

## Watch mode

`pymapper check --watch` checks once, then keeps the loaded program in memory and checks again
whenever a file of the project changes, like `ruff check --watch`. Each cycle prints a full
report (on a terminal the screen is cleared first) and one status line on stderr:

```
$ pymapper check --watch
pkg/a.py:4:5: PM401 function `pkg.a:unused` is never used
Found 1 finding in 1 file (3 files checked).
pymapper: watching /path/to/project for changes (Ctrl-C to stop); checked in 96 ms
...
pymapper: 1 file(s) changed; updated in 6 ms, reported 58 ms after the first event. Watching for changes
```

```
  edit ──> FSEvents / inotify ──> debounce (30 ms quiet, 300 ms max) ──> classify
       ──> incremental update (re-extract changed files, re-resolve their readers)
       ──> analyses + report        (pyproject.toml / pymapper.toml changed: load again)
```

- Changes are found by pymapper's own watcher (`pymapper_core::watch`: FSEvents on macOS,
  inotify on Linux) on the project root. Saved Python files are re-extracted; new, deleted and
  renamed files and directories (and `.gitignore` changes) walk the project again; a changed
  `pyproject.toml` or `pymapper.toml` reloads everything, configuration included. A changed
  baseline, coverage report or config-evidence file is read again.
- Every analysis runs again on the updated program (they are whole-program: one edit can make
  code elsewhere dead or close a cycle); only loading and resolution are incremental.
- Not watched: third-party and stdlib files (reload by touching `pyproject.toml` after an
  install). Duplication (`PM501`, `analysis.dupes`) is not run in watch mode (warned at
  start): the update does not maintain duplication fingerprints. Changing `threads` needs a
  restart.
- A configuration error after a change is printed and the next change retried; errors of the
  first load exit 2 as usual. `--watch` conflicts with `--save-baseline`. The process ends on
  Ctrl-C, or when stdout is closed (`check --watch | head`), with the exit code of the last check.
- `--stats` prints the stats block per cycle plus one `update:` line (files re-extracted,
  added, removed, files re-resolved, phase times).

Cost: memory is that of a `check` run plus the dependency tracking of the incremental
resolution (4 bytes per (file, file read) pair; 3.4 MB on a 17M-line venv). Latency is
measured by `cargo xtask bench -t watch` ([benchmarks.md](benchmarks.md#watch-mode-s77)).

# Query subcommands

Questions about the code for agents and humans: who calls what, what a change can break, how
two symbols connect, what a module imports, where the cycles are, what the codebase looks like.
Every query loads the program exactly as `check` does (same project root, configuration,
import roots, walk, cache and configuration flags: `--deps`, `--stdlib`, `--venv`, `--exclude`,
`--set`, `--config`, `--isolated`, `--cache-dir`, `--no-cache`, `-j`), resolves imports and
calls, and answers from the core query API (the one the GUI uses).

```
pymapper callers SYMBOL [PATHS]...   [--depth N | --transitive] [--edges LEVEL] [--no-overrides] [--scope S] [--limit N]
pymapper callees SYMBOL [PATHS]...   (as callers)
pymapper impact  SYMBOL [PATHS]...   [--depth N] [--edges LEVEL] [--no-overrides] [--scope S] [--limit N]
pymapper path    FROM TO             [--imports] [--edges LEVEL] [--no-overrides] [--node] [--max-len N] [--root DIR]
pymapper imports SYMBOL [PATHS]...   [--depth N] [--direction in|out] [--runtime] [--limit N]
pymapper cycles  [PATHS]...          [--symbol SYMBOL [--calls]] [--edges LEVEL] [--no-overrides] [--limit N]
pymapper summary [PATHS]...          [--top N] [--include-tests] [--all-origins]
```

Every query also takes `--format concise|json` (`sarif` and `github` are usage errors) and
`--stats` (phases `query.build`, `lookup`, `query`, `output`, `output.sites` and peak RSS on
stderr).

## Symbols

`SYMBOL` is looked up in this order, the first form that matches wins:

1. the exact qualified id: `pkg.mod:Class.method`, `pkg.mod`, `@distribution`, with `#N`
   suffixes for same-named siblings (as in `check` findings and baselines);
2. the dotted name `pkg.mod.Class.method` (the longest prefix naming a module is the module);
3. the last segments of qualified ids (`Class.method`, `method`, `mod.Class.method`): unique
   means found, several means ambiguous;
4. nothing matched: similar names (case-insensitive equal, prefix, substring, then a small edit
   distance) as suggestions.

An **ambiguous** symbol lists its candidates on stdout, one per line
(`app/util.py:1:5: app.util:helper (function)`; third-party and stdlib candidates are tagged),
first-party first, and exits 2 with `pymapper: error: `helper` is ambiguous: 2 matches (listed on
stdout); pass a qualified id such as `app.cli:helper`` on stderr. An **unknown** symbol lists
similar names the same way and exits 2. With `--format json` stdout holds one document:
`{"version":2,"tool":{...},"query":"callers","error":"ambiguous"|"not-found","input":"helper","candidates":[...]}`
with `symbol`, `kind`, `path`, `line`, `col`, `origin` and `match` (`suffix`, `fuzzy`) per
candidate.

A package, module or class stands for everything in it (`--scope auto`, the default): a
module's callers are the callers of any of its functions and of its module-level code, and
calls inside it are never reported. `--scope node` uses the node alone, `--scope subtree` the
subtree of a function too (its nested functions).

## Output

Concise output is one fact per line, then one summary line. A line starts with the location it
is about (`path:line:col: `; paths relative to the project root, 1-based lines, byte columns),
names symbols by their qualified id and tags call edges with their confidence:

- `[certain]` statically determined, `[likely]` from a hint (`x = Foo()`, an alias),
  `[possible]` a guess (a unique method name);
- `, dispatch` when only an override edge links the two (a base method to the method that
  overrides it: dynamic dispatch, not a call in the source).

Import facts (`imports` rows, `impact` importers, import edges of `path --imports` and import
cycles) start at the import statement and have no confidence tag: an import edge is a fact of
the source, not a guess. They carry their flags instead (`type-only`, `conditional`, `try`,
`lazy`, `star`).

`--edges certain` (or `likely`) follows only edges at least that sure; `--no-overrides` drops
dispatch edges. JSON follows `check`'s conventions: one document, `version` 2, `tool`, the
`query` name, one result object per line, a `summary`; paths relative to the project root,
`null` for unknown lines and columns (packages and modules have no line), invalid UTF-8
replaced by U+FFFD. A closed stdout (`| head`) stops output quietly.

Positional `PATHS` filter the rows, never what is loaded: the program, its module names and
every walk are the whole project's, so `pymapper callers helper src/api` prints exactly the rows
of `pymapper callers helper` that lie under `src/api`, and the summary line counts the rest
(`; 4 outside PATHS not shown`). A row is under the paths when the file of the symbol it lists
is (the caller for `callers` / `impact`, the callee for `callees`, the other module for
`imports`, any member for `cycles`; `summary` ranks only code under them). As for `check`, a
path covering the project root filters nothing, a path that does not exist is a usage error,
and config discovery starts at the first path.

`--limit N` (default 200) caps the rows printed, closest first; `--limit 0` prints every row.
`impact` applies one cap to its call rows and importers together (call rows first, importers
with what is left); `imports` caps each direction separately. Summary counts always cover every
row.

## Exit codes

| code | meaning |
|------|---------|
| 0 | answered (an empty answer included: no callers, no cycles) |
| 1 | `path` found no path |
| 2 | usage error, ambiguous or unknown symbol, invalid configuration, a path that does not exist, unsupported format, fatal I/O error |

## `callers` / `callees`

Breadth-first over the call graph, `--depth N` hops (default 1), `--transitive` for every hop.
Each reached symbol is one line at its first call site, with the number of further sites to the
same symbol, and for hops beyond the first the symbol it reaches the target through:

```
$ pymapper callers app.util:helper --depth 2
app/main.py:6:9: app.main:run [certain] +1 site
app/util.py:7:16: app.util:Base.run [certain]
tests/test_util.py:5:12: tests.test_util:test_helper [certain]
app/cli.py:10:12: app.cli:main [certain] depth 2 via app.main:run
app/main.py:11:5: app.main (module level) [certain] depth 2 via app.main:run
5 callers of app.util:helper (function, app/util.py:1), up to depth 2

$ pymapper callees app.main:run
app/main.py:7:27: app.core.engine:start [certain] def app/core/engine.py:4:5
app/main.py:6:9: app.util:helper [certain] +1 site def app/util.py:1:5
2 callees of app.main:run (function, app/main.py:5)
```

(Rows are in breadth-first order, by depth, then by symbol order in the index.)

`(module level)` marks a module's top-level code (it runs on import). A dispatch site is the
overriding definition. JSON results: the symbol fields (`symbol`, `kind`, `path`, `line`,
`col`), `confidence` (the edge), `path_confidence` (the least sure edge back to the target),
`dispatch`, `depth`, `via`, `site` (`{path, line, col}`) and `sites`; `summary` has `results`,
`shown`, `outside_paths` and `truncated`.

## `impact`

Everything that transitively calls the symbol (`--depth N` to bound it), i.e. what a change to
it can break, then the modules that import its module directly or transitively (every import
edge, type-only and lazy ones included). Rows are tagged with the least sure edge on the path
back to the symbol, so `[possible]` means one hop of the chain is a guess:

```
$ pymapper impact app.util:helper
app/main.py:6:9: app.main:run [certain] +1 site
...
app/cli.py:10:12: app.cli:main [certain] depth 2 via app.main:run
app/main.py:11:5: app.main (module level) [certain] depth 2 via app.main:run
app/cli.py:5:26: app.cli [importer] imports app.util
app/main.py:1:22: app.main [importer] imports app.util
tests/test_util.py:1:22: tests.test_util [importer] imports app.util
impact of app.util:helper (function, app/util.py:1): 5 affected (4 symbols, 1 module level), 1 in tests, 4 files; 3 importing modules; most affected: app.main 2, app.cli 1, app.util 1
```

An importer line sits at its first import statement of the module that brought it in: one of
the symbol's modules (its own module for a class or function) for a direct importer, else an
importer listed before it (`app/cli.py:9:26: app.cli [importer] imports app.main` under
`impact app.core.db`), with `+N rows` when more statements import that module. Counts cover
the whole program, positional paths only filter the rows. JSON adds `importers` (the symbol
fields, `imports`, `direct`, `site` (`{path, line, col}`) and `rows`), `modules` (per
package/module: `affected`, most first) and `summary` with `symbols`, `module_scopes`, `tests`,
`files`, `importers`.

## `path`

A shortest chain from `FROM` to `TO`: calls by default, `--imports` for module imports (a
class or function stands for its module). One line per edge, at the call site or the import
statement that makes it; exit 1 when there is none (`--max-len N` bounds the search). There are
no positional paths: a chain is one fact. Config and project-root discovery start at the working
directory, or at `--root DIR` to query a project from outside it.

```
$ pymapper path app.cli:main app.util:helper
app/cli.py:10:12: app.cli:main -> app.main:run [certain]
app/main.py:6:9: app.main:run -> app.util:helper [certain]
call path from app.cli:main (function, app/cli.py:8) to app.util:helper (function, app/util.py:1): 2 edges, least sure certain
```

Import edges carry their flags: `[import, type-only, conditional]`, `lazy` (inside a function),
`try`, `star`; an edge with several import statements shows the flags all of them share. JSON:
`graph`, `from`, `to`, `found`, `nodes` (symbol objects), `edges` (`from`, `to`, `confidence` and
`dispatch` or `flags`, `site`) and `summary` (`length`, `worst`).

## `imports`

Module-level import dependencies of a package, module, class or function: what it imports
(`out`) and what imports it (`in`; nothing imports a class or function, so those have only
`out`, the import statements inside them). A package covers every module below it, and imports
between its own modules are not listed. Imports of code that is not loaded (the stdlib without
`--stdlib`, third-party code without `--deps`, unknown modules) are listed by top-level name.
Every line sits at the first import statement behind it, where it can be removed or moved: in
the symbol's files for `out` (followed by `def path` of the imported module when it is loaded),
in the importer's file for `in`. `(N rows)` counts the statements:

```
$ pymapper imports app.cli
app/cli.py:9:26: imports app.main (1 row, lazy) def app/main.py
app/cli.py:5:26: imports app.util (1 row, type-only, conditional) def app/util.py
app/cli.py:1:8: imports os (stdlib, not loaded, 1 row)
app/cli.py:2:20: imports typing (stdlib, not loaded, 1 row)
imports of app.cli (module, app/cli.py): 4 out (2 not loaded), 0 in

$ pymapper imports app.core.db --direction in
app/core/engine.py:1:22: imported by app.core.engine (1 row)
imports of app.core.db (module, app/core/db.py): 1 in
```

With `--runtime`, a line whose module is also imported under `if TYPE_CHECKING:` sits at a
runtime statement. `--direction in` on a class or function is a usage error (exit 2).

`--depth N` rolls the other side up to its package of N dotted components (`--depth 1`:
top-level packages), never to a package containing the symbol itself: a sibling stays at its
shallowest ancestor outside the symbol's package (`imports app.main --depth 1` lists
`app.core`, not `app`). Rows are heaviest first (import statements); `--direction in|out` keeps
one side, `--runtime` skips imports only type checkers see. JSON rows have `direction`, the
symbol fields (where the other module is), `origin`, `rows`, `flags` and `site` (`{path, line,
col}` of the import statement, `null` if unknown), or `external`, `origin` (`stdlib`,
`third-party`, `unknown`), `rows` and `site` for code that is not loaded. Positional paths
select the other module, so imports of code that is not loaded are listed only without them.

## `cycles`

Every import cycle of the project: the cyclic strongly connected components of the graph PM301
checks (first-party runtime imports; `analysis.type-checking-cycles` keeps type-only ones), one
line each at the import that starts its witness, a shortest cycle through the component (no
confidence tag: import edges are facts of the source):

```
$ pymapper cycles
app/core/engine.py:1:22: import cycle (2 modules): app.core.engine -> app.core.db -> app.core.engine
1 import cycle
```

`--symbol SYMBOL` keeps the components holding the symbol's modules; `--symbol SYMBOL --calls`
reports the call cycle through it instead (direct or mutual recursion, following `--edges` /
`--no-overrides`), tagged with its least sure edge. JSON results: `size`, `members` (every
module or function of the component), `witness`, `site` and, for calls, `worst`; `summary`:
`cycles`, `largest`.

## `summary`

The codebase at a glance: per origin files, lines, packages, modules, classes and functions;
import rows, unresolved imports and module edges; call sites by confidence and distinct
caller-callee pairs; import cycles; then the top `--top N` (default 10) functions by cognitive
and cyclomatic complexity, the most called classes and functions (distinct real callers) and
the most imported modules. Rankings skip tests (`--include-tests`), third-party and stdlib code
(`--all-origins`), and entries of 0; positional paths restrict the rankings, never the counts.

```
$ pymapper summary
first-party: 630 files, 304805 lines, 46 packages, 586 modules, 2565 classes, 15557 functions
imports: 4471 rows, 48 unresolved (1.1%), 2618 module edges
calls: 60154 sites: 32.9% certain, 5.2% likely, 4.1% possible, 57.9% unresolved; 25290 caller-callee pairs
import cycles: 9 (largest: 223 modules)
re/_parser.py:511:5: cognitive re._parser:_parse 422
...
posixpath.py:72:5: most called posixpath:join (155 callers)
os.py: most imported os (156 importers)
```

(The CPython 3.13 standard library loaded as a project.) JSON: `origins`, `imports`, `calls`,
`import_cycles`, and one array per ranking (`cognitive`, `cyclomatic`, `most_called`,
`most_imported`) of symbol objects with a `value`.

## Cost

A query costs the load `check` pays (walk, cached facts, import and call resolution) plus
building the query's call graph (O(V + E)); the query itself visits only what it reports. On
the 17.8M-LOC `aiml-lectio` project with `--deps --stdlib`, warm cache, a query runs in
0.41-0.45 s of which the query graph is 35 ms and the query below 35 ms; printing 284k
transitive callers of `warnings:warn` with `--limit 0` adds 0.34 s. Numbers:
[`benchmarks.md`](benchmarks.md#query-subcommands-s52).
