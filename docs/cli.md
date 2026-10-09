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

The PM6xx dependency-hygiene rules belong to the `deps` step; `check` does not run them (see
"Rules that cannot run" below).

`pymapper deps` (third-party distributions, their use by first-party code and `--score`
scorecards) is described in [`deps.md`](deps.md). The other subcommands (`callers`, `callees`,
`impact`, `path`, `cycles`, `summary`) are not implemented yet: they are hidden from
`pymapper --help`, accept any operands and the configuration flags, and exit 2 with "not
implemented".

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
| `--preset` | `presets` | flask, fastapi, django, click, pytest, pydantic |
| `--exclude GLOB` | `extend-exclude` | gitignore syntax, relative to the project root |
| `--set 'KEY = VALUE'` | any | TOML fragment, e.g. `--set 'analysis.dupes = true'` |
| `--config FILE`, `--isolated` | | explicit config file / ignore config files |
| `--baseline FILE` | `baseline` | report only findings not in the baseline |
| `--save-baseline FILE` | | record every reported finding, write no report, exit 0 |
| `--stats` | | phase timings, counters and peak RSS on stderr |

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

The default cache directory is `.pymapper_cache` in the project root (see "Project root").
So `pymapper check /some/dir` with no config and no marker above it writes
`/some/dir/.pymapper_cache`, even from another working directory (pointing it at an interpreter's stdlib would write into
the Python installation). Use `--cache-dir DIR` or `--no-cache` for directories you do not
own; a cache that cannot be written is only a warning.

## Rules that cannot run

A selected rule does not run in `check` when its `[analysis]` toggle is off (PM501 needs
`analysis.dupes = true`), when it is a dependency rule (PM6xx, owned by `pymapper deps`), or when
it is a contract rule (PM302-PM304) and no `[[contracts]]` are configured.

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
  17 rules ran, 8 skipped (PM302-PM304: no [[contracts]]; PM501: needs `analysis.dupes = true`; PM601-PM604: run by `pymapper deps`, not `check`).
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
