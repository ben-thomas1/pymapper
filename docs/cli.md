# `pymapper check`

`pymapper check [PATHS]...` loads a project, runs every analysis that has an enabled rule and
reports findings, ruff-style. It is meant to run in CI and in agent loops next to pytest, ruff
and ty.

## Pipeline

```
pymapper check [paths]
  │
  ├─ config        pyproject.toml [tool.pymapper] / pymapper.toml, --set, flags     phase config
  ├─ roots         import roots: src layout, venv, stdlib, .pth, dist-info          phase roots
  ├─ files         first-party walk (gitignore + exclude), stdlib walk,             phase walk
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
  ├─ filter        selection, per-file-ignores, # pymapper: ignore[...], confidence gate
  ├─ report        deterministic order (path, line, col, rule, symbol)
  ├─ baseline      drop findings recorded in --baseline / `baseline`
  └─ output        concise | json | sarif | github, then the exit code
```

Third-party (`--deps`) and stdlib (`--stdlib`) code is loaded so first-party code links to it
(imports, base classes, calls), but it is never reported by `check` and never affects the exit
code. Only first-party importable `.py` files are analysed for findings.

The PM6xx dependency-hygiene rules belong to the `deps` step; `check` does not run them yet.

## Exit codes

| code | meaning |
|------|---------|
| 0 | no finding at or above the confidence gate (or `--save-baseline` succeeded) |
| 1 | at least one finding at or above the gate |
| 2 | usage error, invalid configuration, unreadable or invalid baseline, cannot write output |

Unreadable files and directories, a missing stdlib or venv, and a cache that cannot be written
are warnings on stderr, never fatal. A closed stdout (`pymapper check | head`) stops output
quietly; the exit code still reflects the findings.

## Flags

Positional `PATHS` replace the configured `paths`; config discovery starts at the first one
(default: the working directory). Every configuration flag is shared with the other
subcommands (`pymapper check --help` lists them all):

| flag | config key | notes |
|------|------------|-------|
| `--select`, `--extend-select`, `--ignore` | `rules.*` | comma-separated selectors: `ALL`, `PM1`, `PM10`, `PM101` |
| `--format` / `--output-format` | `output-format` | `concise` (default), `json`, `sarif`, `github` |
| `--confidence` / `--min-confidence` | `min-confidence` | 0-100, default 80; lower shows less certain findings |
| `--deps` / `--no-deps` | `include-deps` | load the venv's third-party packages |
| `--stdlib` / `--no-stdlib` | `include-stdlib` | load the interpreter's standard library |
| `--venv DIR` | `venv` | default `$VIRTUAL_ENV`, then `.venv` / `venv` |
| `--no-cache`, `--cache-dir DIR` | `cache`, `cache-dir` | default `.pymapper_cache` in the project root |
| `--threads N` / `-j N` | `threads` | 0 = one per logical CPU |
| `--mode` | `mode` | dead code: `auto`, `application`, `library` |
| `--preset` | `presets` | flask, fastapi, django, click, pytest, pydantic |
| `--exclude GLOB` | `extend-exclude` | gitignore syntax, relative to the project root |
| `--set 'KEY = VALUE'` | any | TOML fragment, e.g. `--set 'analysis.dupes = true'` |
| `--config FILE`, `--isolated` | | explicit config file / ignore config files |
| `--baseline FILE` | `baseline` | report only findings not in the baseline |
| `--save-baseline FILE` | | record every reported finding, write no report, exit 0 |
| `--stats` | | phase timings, counters and peak RSS on stderr |

Selecting a gated rule by code or category (`--select PM501`) without its toggle prints a
warning naming the toggle (`analysis.dupes`).

## Output formats

- `concise`: `path:line:col: CODE message`, confidence appended when below 100, then a footer
  (`Found N findings in M files (K files checked).`, counts of suppressed / baselined /
  below-gate findings). Paths are relative to the project root; columns are 1-based bytes.
- `json`: one document (`version`, `tool`, `min_confidence`, `findings[]`, `summary`); every
  finding has `code`, `name`, `severity`, `confidence`, `path`, `line`, `col`, `symbol` (the
  stable qualified id `pkg.mod:Class.method`) and `message`.
- `sarif`: SARIF 2.1.0 with rule metadata, for code-scanning uploads.
- `github`: GitHub Actions workflow commands (`::warning file=...,line=...::message`).

## Suppressions, per-file ignores, baselines

- Inline: `# pymapper: ignore[PM401]` (or `ignore[PM4]`, or bare `# pymapper: ignore`) on the
  line a finding is reported at.
- Per file: `[rules.per-file-ignores]` maps gitignore-style globs (relative to the project
  root) to selectors: `"tests/" = ["PM4"]`. A pattern matching a parent directory covers every
  file below it.
- Baselines: `pymapper check --save-baseline .pymapper-baseline.json` records today's findings
  by rule + stable symbol id + stable message arguments (never line numbers or metric values),
  so editing a function does not resurface its finding. Then `--baseline FILE` (or
  `baseline = "FILE"` in the config) reports only new findings. The file is JSON, sorted,
  and written atomically; commit it.

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
  loc                      304805
  ...
  cache_hits                    0
  cache_misses                630
  peak_rss                   24.0 MB
```

Phases are wall time on the coordinating thread, in first-recorded order, with sub-phases
recorded by the core library (`load.*`, `resolve.*`, `deadcode.*`, `dupes.*`). `peak_rss` is
`getrusage` `ru_maxrss` of the whole process. Measured numbers are in
[`benchmarks.md`](benchmarks.md#check-s45).
