# Using pymapper

pymapper analyses a whole Python project without running it. It reads every file, resolves
imports and calls across modules, and then reports problems that single-file linters such as
ruff and ty cannot see. It has three parts:

| Part | What it does |
|---|---|
| `pymapper check` | finds problems and exits non-zero when it finds any, like ruff or pytest |
| query commands | answer structural questions: who calls this, what breaks if I change it, how A reaches B |
| `pymapper-gui` | a native map of the codebase where you can see packages, modules, the links between them and quality metrics |

All examples below use the small sample project in `tests/corpus/sample_python`, with a
`pyproject.toml` added. The output shown is real output from pymapper 0.3.0.

## Contents

- [Running pymapper](#running-pymapper)
- [Concepts you need first](#concepts-you-need-first)
- [pymapper check](#pymapper-check)
- [Query commands](#query-commands)
- [pymapper deps](#pymapper-deps)
- [pymapper-gui](#pymapper-gui)
- [Configuration](#configuration)
- [Recipes](#recipes)
- [Troubleshooting](#troubleshooting)

## Running pymapper

From a checkout of this repository:

```sh
cargo run --release -- check ~/code/myproject                    # the CLI (default binary)
cargo run --release -p pymapper-gui -- ~/code/myproject          # the GUI
```

Everything after `--` is passed to pymapper. Always use `--release`: debug builds are much
slower. To run pymapper from inside the project you are analysing instead:

```sh
cd ~/code/myproject
cargo run --release --manifest-path ~/code/pymapper/Cargo.toml -- check
```

You can also install both binaries onto your `PATH`:

```sh
cargo install --path crates/pymapper-cli
cargo install --path crates/pymapper-gui
```

The rest of this guide writes `pymapper` and `pymapper-gui` for short.

## Concepts you need first

### The whole project is always analysed

pymapper first finds the **project root**. It walks up from the first path you give (or from
the current directory) and stops at the first of these that it finds:

1. the directory of a `pymapper.toml`, or of a `pyproject.toml` with a `[tool.pymapper]` table
2. the outermost directory whose `pyproject.toml` has `[tool.uv.workspace]`
3. the top of the git repository
4. the outermost directory with a `pyproject.toml`

It then analyses everything under that root. The paths you pass on the command line only
**filter what is reported**. They never change what is analysed. So
`pymapper check src/myapp/utils.py` reports on one file but still knows about every caller in
the whole repository. When you filter, the output says so:

```
Filtered to src/myapp/utils.py (1 of 1803 first-party files); the whole project was analysed.
```

### Symbols

Symbols are written `package.module:Class.method`, for example
`services.auth:create_authenticator.authenticate`. The part before the colon is the module,
and the part after it is the path inside that module. Wherever a command asks for a symbol you
can give the full id, a dotted name, or just the last few segments, as long as that is
unambiguous. If it is ambiguous, pymapper lists the candidates and exits with code 2.

### Confidence

Every finding has a confidence from 0 to 100.

- **Measured facts are always 100.** This covers complexity, size, coupling, import cycles and
  contract violations.
- **Dead-code and dependency findings are scored.** Python is dynamic, so pymapper lowers the
  confidence whenever it sees evidence that something might be used after all. For example,
  the name appears in a string, the module uses `getattr`, or a YAML file mentions it.

`check` reports only findings with confidence of at least 80 (change this with
`--min-confidence`). Anything below 100 says why it was lowered:

```
services/auth.py:2:9: PM401 function `services.auth:require_auth.wrapper` has no resolved callers (name appears in its own file) (confidence 10: name-in-same-file)
```

### Library vs application mode (dead code only)

Dead-code analysis needs to know where execution starts.

- **Application mode:** only entry points are roots. Entry points are `__main__` guards,
  `__main__.py` files, `[project.scripts]` and tests. Anything that cannot be reached from them
  is unused.
- **Library mode:** public names are also roots, because external code calls them. Only private
  or truly unreachable code is reported.

`mode = "auto"` (the default) decides separately for **each package**. A package is every
directory that has its own `pyproject.toml`. A package with scripts, or with a plausible
`__main__`, counts as an application; anything else is a library. `__main__` guards inside
tests, scripts, examples and private helpers are ignored. `check --stats` prints each decision
and the reason for it:

```
deadcode.mode .: library (no [project.scripts] and no plausible __main__ guard)
```

Force a mode with `--mode application` or `--mode library`.

### The cache

pymapper caches what it extracts from each file, so repeat runs only re-read files that have
changed. A warm run on a 480k-line monorepo takes about 0.15 s. By default the cache is
`<project root>/.pymapper_cache/`, which contains its own `.gitignore`, so git never sees it.
Use `--cache-dir DIR` to put it somewhere else, or `--no-cache` to skip it.

## pymapper check

```sh
pymapper check                       # the whole project
pymapper check src/myapp             # whole project analysed, report filtered to src/myapp
pymapper check --deps                # also load the virtualenv (better resolution, slower cold)
```

### Reading the output

```
$ pymapper check
middleware/pipeline.py:6:25: PM405 `User` is imported but never used
Found 1 finding in 1 file (18 files checked).
Not shown: 4 below min-confidence.
```

Each finding is one line, in the same shape as ruff:

```
middleware/pipeline.py:6:25: PM405 `User` is imported but never used
└──── path ──────────┘ │  │  └ rule └──────── message ───────────┘
                     line col
```

- **path:line:col** is relative to where you ran pymapper, and is clickable in most editors and
  terminals.
- **Rule code** identifies the kind of problem (see the table below).
- **Message** names the symbol and, for metrics, the value against its threshold, e.g.
  `` `services.auth:make_validator` is too complex (McCabe 4 > 3) ``.
- **Confidence suffix** appears only when the confidence is below 100, e.g.
  `(confidence 10: name-in-same-file)`.

The footer gives totals and explains anything that is not shown:

| Footer line | Meaning |
|---|---|
| `Found N findings in M files (K files checked).` | the totals |
| `Not shown: 4 below min-confidence.` | findings hidden by the confidence gate; add `--min-confidence 0` to see them |
| `Not shown: 1 baselined ...` / `... suppressed` | findings hidden by a baseline or by suppression comments |
| `Filtered to <paths> ...` | you passed paths, so only part of the project was reported |
| `17 rules ran, 8 skipped (...)` | printed when you choose rules explicitly; says which rules could not run and why |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | no findings at or above the confidence gate |
| 1 | there are findings |
| 2 | usage or fatal error: unknown rule selector, bad config, no runnable rule selected, etc. |

### The rules

| Rules | Family | What it shows you |
|---|---|---|
| PM101–PM107 | complexity and size | functions too complex (McCabe, cognitive), too deeply nested, too long, with too many parameters; modules too long |
| PM201–PM204 | coupling | modules importing too much, god modules, god classes |
| PM301 | import cycles | modules that import each other, with the cycle size and an example path |
| PM302–PM304 | layering contracts | imports that break the architecture rules you declare in config |
| PM401–PM405 | dead code | unused functions, classes, methods, modules and imports, scored by confidence |
| PM501 | duplication | copy-pasted code blocks (opt in with `--set 'analysis.dupes = true'`) |
| PM601–PM604 | dependency hygiene | run by `pymapper deps --check`, not `check` (see below) |

Default thresholds: McCabe 10, cognitive 15, nesting 4, function 100 lines, 7 parameters,
module 1000 lines. Function and module length exclude docstrings. Test code
(`tests/`, `test_*.py`, `conftest.py`, ...) is left out of the complexity, coupling and
duplication rules by default, but dead code and cycles still cover it. `docs/rules.md` has
every rule with its exact definition and options.

Examples of each family:

```
services/auth.py:34:5: PM101 `services.auth:create_authenticator` is too complex (McCabe 6 > 3)
services/auth.py:34:5: PM102 `services.auth:create_authenticator` is hard to understand (cognitive complexity 8 > 3)
app/infra/db.py:1:30: PM301 import cycle (2 modules): app.infra.db -> app.domain.model -> app.infra.db
app/domain/model.py:1:26: PM303 `app.domain.model` imports forbidden module `app.infra.db` (contract `domain is independent of infra`)
app.py:27:5: PM401 function `app:dead_endpoint` is never used
repo/product_repo.py:1:1: PM404 module `repo.product_repo` is never imported
```

(The PM1 lines above were produced with `--set 'thresholds.max-mccabe = 3'` and `--set 'thresholds.max-cognitive = 3'` to get output on the small sample project.)

### Dead-code wording and reasons

The wording tells you how strong the evidence is:

| Message | Meaning |
|---|---|
| `is never used` / `is never imported` | nothing refers to it at all: the strongest case |
| `is only used by unused code` / `is only imported by unused modules` | something refers to it, but that code is itself dead, so remove both |
| `has no resolved callers (...)` | no call was found, but the name appears somewhere; the brackets say where |

These are the reasons you will meet most often in the confidence suffix and the JSON
`reasons` array:

| Reason | Meaning |
|---|---|
| `no-references` | nothing mentions it |
| `only-used-by-unused-code` | it is used only from dead code |
| `name-in-same-file`, `name-in-importer`, `name-elsewhere` | the name appears as text in its own file, in a file that imports it, or anywhere else; it may be a dynamic use |
| `string-reference` | a `"pkg.mod:attr"` or `"pkg.mod"` string names it (factories, plugin paths) |
| `config-reference` | a YAML/TOML/JSON/INI file names it |
| `used-only-in-tests` | only test code uses it |
| `possible-call-only` | the only caller is a guess (method name matched, receiver type unknown) |
| `module-dynamic-access`, `getattr-prefix` | the module looks names up dynamically (`getattr(x, name)`, `f"_dt_{x}"`) |
| `unknown-decorator` | a decorator pymapper does not know may register it |
| `external-base`, `external-override` | it overrides or extends a third-party class, which may call it |
| `library-public-not-in-all` | it is public in a library package but not listed in `__all__` |

### More output for diagnosis

```sh
pymapper check --min-confidence 0          # everything, including weak guesses
pymapper check --stats                     # timings, counters, peak memory, skipped rules, mode per package
```

`--stats` ends with the information you need when results look wrong:

```
  rules_run                    15
  rules_skipped                 7
  deadcode.units                1
  deadcode.library              1
  peak_rss                    9.9 MB
  skipped PM302-PM304: no [[contracts]] are configured
  skipped PM501: expensive, needs `analysis.dupes = true` (e.g. --set 'analysis.dupes = true')
  skipped PM601-PM603: dependency rules are run by `pymapper deps --check`, not `check`
  deadcode.mode .: library (no [project.scripts] and no plausible __main__ guard)
```

### JSON, SARIF and GitHub output

```sh
pymapper check --output-format json      # for agents and scripts
pymapper check --output-format sarif     # for code-scanning tools
pymapper check --output-format github    # GitHub Actions annotations
```

The JSON looks like this (version 2):

```json
{"version": 2, "tool": {"name": "pymapper", "version": "0.3.0"}, "min_confidence": 0,
 "summary": {"findings": 4, "files": 2, "files_checked": 18, "suppressed": 0, "baselined": 0, ...},
 "findings": [
  {"code": "PM405", "name": "unused-import", "severity": "warning", "confidence": 100,
   "path": "middleware/pipeline.py", "line": 6, "col": 25, "end_line": 6,
   "symbol": "middleware.pipeline:User", "message": "`User` is imported but never used",
   "reasons": ["no-references"]},
  {"code": "PM101", "name": "complex-function", "severity": "warning", "confidence": 100,
   "path": "services/auth.py", "line": 11, "col": 5, "end_line": 18,
   "symbol": "services.auth:make_validator",
   "message": "`services.auth:make_validator` is too complex (McCabe 4 > 3)",
   "value": 4, "threshold": 3}
 ]}
```

Some fields appear only for certain rules:

| Field | Present on |
|---|---|
| `value`, `threshold` | metric rules (PM1xx, PM2xx) |
| `reasons` | dead-code findings with evidence |
| `cycle` | PM301: `size`, all `members` and a short `witness` path |
| `other` | PM501: the other copy's `path`, `line`, `end_line` and `symbol` |

### Suppressing and baselining

You can silence a finding on its own line. The line is the `def`/`class` line, the import
line, or line 1 for module findings:

```python
def handler():  # pymapper: ignore[PM401]
import plugins  # noqa: F401          ← ruff's noqa is honoured for unused imports
```

A bare `# pymapper: ignore` silences every rule on that line. Misspelt codes produce a warning
rather than being silently ignored.

To adopt pymapper on a codebase that already has many findings, save a baseline and from then
on report only new ones:

```sh
pymapper check --save-baseline .pymapper-baseline.json   # record today's findings, exits 0
pymapper check --baseline .pymapper-baseline.json        # report only findings not in it
```

## Query commands

These commands answer questions instead of looking for problems. Their output has one fact per
line, starting with a clickable `path:line:col` and ending with how sure the call is (`[certain]`,
`[likely]`, `[possible]`). Every query command also accepts `--output-format json`.

### summary: a codebase at a glance

```
$ pymapper summary
first-party: 18 files, 239 lines, 5 packages, 13 modules, 3 classes, 39 functions
imports: 24 rows, 0 unresolved (0.0%), 21 module edges
calls: 66 sites: 43.9% certain, 0.0% likely, 12.1% possible, 43.9% unresolved; 36 caller-callee pairs
import cycles: 0
services/auth.py:34:5: cognitive services.auth:create_authenticator 8
services/auth.py:1:5: cognitive services.auth:require_auth 6
...
```

- **Line 1:** the size of your code.
- **Line 2:** how many imports pymapper could resolve. "Unresolved" usually means a missing
  dependency or a dynamic import.
- **Line 3:** how much of the call graph pymapper understands. "Unresolved" calls are mostly
  calls into third-party or builtin code, or calls on values whose type pymapper cannot infer.
  Run with `--deps` to resolve more of them.
- **Line 4:** the number of import cycles.
- **The rest:** the worst hotspots by cognitive complexity, ready to click.

### callers / callees: one step up or down the call graph

```
$ pymapper callers services.user_service:create_user
app.py:10:13: app:list_users [certain]
app.py:22:5: app:main [certain]
middleware/pipeline.py:17:12: middleware.pipeline:run_pipeline [certain]
3 callers of services.user_service:create_user (function, services/user_service.py:9)

$ pymapper callees app:main
app.py:24:5: cycles:ping [certain] def cycles.py:1:5
app.py:23:5: middleware.pipeline:run_pipeline [certain] def middleware/pipeline.py:10:5
...
4 callees of app:main (function, app.py:20)
```

Each line is a call site: where the call happens, the other function involved, and how sure
pymapper is. `callees` also gives the location of the definition (`def path:line:col`).

### impact: what can break if I change this

```
$ pymapper impact utils.validation:validate_email
middleware/pipeline.py:13:5: middleware.pipeline:run_pipeline [certain]
services/user_service.py:11:5: services.user_service:create_user [certain] +4 sites
app.py:23:5: app:main [certain] depth 2 via middleware.pipeline:run_pipeline
app.py:10:13: app:list_users [certain] depth 2 via services.user_service:create_user
middleware/pipeline.py:1:30: middleware.pipeline [importer] imports utils.validation +2 rows
...
impact of utils.validation:validate_email (function, utils/validation.py:1): 4 affected (4 symbols, 0 module level), 0 in tests, 3 files; 3 importing modules; most affected: app 2, middleware.pipeline 1, services.user_service 1
```

This walks the call graph backwards as far as it goes. Direct callers come first; `depth 2 via
X` means the symbol reaches your function through X. `[importer]` rows are modules that import
the target. The last line is the blast radius, including how much of it is test code. This is
the command to run before a refactor.

### path: how does A reach B

```
$ pymapper path app:main repo.user_repo:save_user
app.py:22:5: app:main -> services.user_service:create_user [certain]
services/user_service.py:18:5: services.user_service:create_user -> repo.user_repo:save_user [certain]
call path from app:main (function, app.py:20) to repo.user_repo:save_user (function, repo/user_repo.py:1): 2 edges, least sure certain
```

This gives the shortest call chain, one hop per line. "Least sure" is the weakest link in the
chain: `possible` means at least one hop is a guess.

### imports: who imports a module, and what it imports

```
$ pymapper imports services.auth
app.py:1:27: imported by app (1 row)
middleware/pipeline.py:3:27: imported by middleware.pipeline (1 row)
imports of services.auth (module, services/auth.py): 0 out, 2 in
```

Outgoing imports say where each target is defined, or that it is `stdlib, not loaded` or a
third-party package. Use the rollup option to see dependencies between whole packages
(see `pymapper imports --help`).

### cycles: import cycles and recursion

```
$ pymapper cycles
app/infra/db.py:1:30: import cycle (2 modules): app.infra.db -> app.domain.model -> app.infra.db
1 import cycle

$ pymapper cycles --calls --symbol cycles:ping
cycles.py:2:5: call cycle (2 functions): cycles:ping -> cycles:pong -> cycles:ping [certain]
1 call cycle through cycles:ping (function, cycles.py:1)
```

The size is the number of modules involved in the whole cycle. The path shown is one short
example loop you can break; the JSON output lists every member.

## pymapper deps

`pymapper deps` looks at the third-party packages in your virtualenv. It finds the venv from
`.venv`, `$VIRTUAL_ENV` or `--venv DIR`.

### Which packages you use, and how much of each

```
$ pymapper deps
distribution  version  importers  api used  loc reached
httpx         0.28.1           1         1  2952/8828 (33%)
PyYAML        6.0.3            1         1  656/5923 (11%)
requests      2.34.2           1         6  3580/6385 (56%)
rich          13.9.4           1         2  15002/26607 (56%)

4 of 501 distributions used by first-party code (20 first-party files); --all lists every one.
```

| Column | Meaning |
|---|---|
| `importers` | how many of your modules import it |
| `api used` | how many of its functions and classes your code calls directly |
| `loc reached` | how much of its code your calls can reach: a measure of how deeply you depend on it |

A package where you use 1 function but pull in 5,000 lines is a candidate for replacement or
for writing the code yourself.

### How good is that package's code

```
$ pymapper deps --score
distribution  version  grade  score    loc  hot/kloc    mi   cc  deps  importers  api used  loc reached
httpx         0.28.1   C       65.8   8828       6.9  50.5  2.2    13          1       1/2  2952/8828 (33%)
PyYAML        6.0.3    C       61.2   5923       7.8  51.4  3.3     0          1     1/381  656/5923 (11%)
requests      2.34.2   C       69.8   6385       3.8  48.3  2.9     8          1     6/208  3580/6385 (56%)
rich          13.9.4   D       53.7  26607       4.3  46.0  2.7     6          1     2/586  15002/26607 (56%)
```

| Column | Meaning |
|---|---|
| `grade`, `score` | overall quality, A (best) to F, 0–100 |
| `loc` | size of the package |
| `hot/kloc` | complexity hotspots per 1,000 lines |
| `mi` | average maintainability index (higher is better) |
| `cc` | average cyclomatic complexity per function |
| `deps` | how many other packages it depends on |
| `api used` | functions and classes you call, out of its public API |

Scores are cached per package version, so only the first run is slow. The formula is in
`docs/deps.md`. Scores never affect `check`'s exit code.

### Dependency hygiene

```
$ pymapper deps --check
clients/http.py:3:8: PM601 `yaml` is provided by `PyYAML`, which is not a declared dependency
clients/http.py:4:6: PM601 `rich.console` is provided by `rich`, which is not a declared dependency
pyproject.toml:4:38: PM602 dependency `pydantic` is declared but never imported
Found 3 findings in 2 files (20 files checked).
```

This compares what you import with what `pyproject.toml` declares. It finds undeclared
imports (PM601), unused dependencies (PM602), imports of a package's private internals
(PM603) and, if enabled, imports of packages you only get transitively (PM604). It uses the
same formats, suppressions, baselines and exit codes as `check`.

## pymapper-gui

```sh
pymapper-gui ~/code/myproject              # your code
pymapper-gui --deps ~/code/myproject       # include the virtualenv, drawn greyed out on the right
pymapper-gui --watch ~/code/myproject      # the map updates as you edit
```

### What you see

```
┌──────────────┬──────────────────────────────────────────┬──────────────┐
│ search       │  code map (canvas)                       │ details      │
│ + tree       │  ┌──────────── myapp ──────────────┐ ┌──┐│ map controls │
│              │  │ ┌─ services ─┐ ┌─ models ─┐     │ │3p││ selection    │
│              │  │ │ auth  user │ │ base     │ ... │ │  ││ callers/     │
│              │  │ └────────────┘ └──────────┘     │ │  ││ callees      │
│              │  └─────────────────────────────────┘ └──┘│ source       │
│              │  legend (bottom left)                    │              │
├──────────────┴──────────────────────────────────────────┴──────────────┤
│ findings                                                               │
└────────────────────────────────────────────────────────────────────────┘
```

- **Cells.** Every rectangle is a package, module, class or function, nested the way your code
  is. The **area** shows size: lines of code by default, or symbols, total complexity, or
  uniform. The **colour** shows a metric you choose: kind, maximum cyclomatic or cognitive
  complexity, complexity per 100 lines, lines or symbols. The legend at the bottom left shows
  the scale. The layout is deterministic, so your code is in the same place every time you
  open it.
- **Edges.** Curves are calls (yellow at the caller, pink at the callee) or imports (blue to
  violet). They are bundled along the package hierarchy, so a thick bundle between two packages
  means heavy traffic between them. You choose what thickness means: call sites, distinct
  caller/callee pairs, lines of code reached, API surface used, complexity-weighted, or
  imports. Faint edges are calls pymapper is only guessing at.
- **Semantic zoom.** At the start you see top-level packages, with edges summed between them.
  As you zoom in, packages open into modules, then classes and functions, and the edges split
  to match.
- **Third-party code** (with `--deps`) sits in a greyed region on the right. Edges into it stop
  at the package level.
- **Overlays.** Toggles in the panel add import cycles (members outlined in red, the cycle
  drawn as arcs), layering-contract violations, and finding badges coloured by severity.
- **Hover** a cell to see its id, kind, lines, symbols and complexity. Hover an edge to see both
  ends and every edge metric.
- **The details panel** shows the selection's metrics, its callers and callees (click to jump),
  its findings and its source code. Source is read from disk on demand.

### Controls

| Action | Control |
|---|---|
| Pan | drag, or two-finger scroll on a trackpad |
| Zoom | pinch, mouse wheel, or Cmd/Ctrl+scroll |
| Fit the whole map | Home, or double-click empty canvas |
| Select | click a cell |
| Select and zoom to it | double-click a cell |
| Shortest path from the selection | Shift-click another cell |
| Clear selection and path | Escape, or click empty canvas |
| Search | type in the left box (fuzzy: `tngl`, or a qualified prefix like `core.models:Base`), then Enter |

`pymapper-gui --measure <path>` opens the map, prints a frame-time and idle-CPU report, and
exits. More detail is in `docs/gui.md`.

## Configuration

Settings go in `pymapper.toml`, or under `[tool.pymapper]` in `pyproject.toml`. Print every
option, with comments and defaults:

```sh
pymapper config --template   # the commented template
pymapper config              # the effective settings, the project root and the config file used
```

Settings you are likely to want:

```toml
[tool.pymapper]
mode = "auto"                 # dead code: auto | application | library
min-confidence = 80
presets = ["fastapi", "pytest"]   # flask, fastapi, django, click, pytest, pydantic

[tool.pymapper.rules]
ignore = ["PM105"]
per-file-ignores = { "scripts/**" = ["PM1"] }

[tool.pymapper.thresholds]
max-mccabe = 12

[tool.pymapper.entrypoints]
names = ["myapp.plugins.tools:*"]   # functions a framework loads by name from config files

[[tool.pymapper.contracts]]
name = "domain is independent of infra"
kind = "forbidden"            # also: layers, independence
source = ["app.domain"]
forbidden = ["app.infra"]
```

Any option can be set for one run with `--set`, e.g. `--set 'thresholds.max-mccabe = 12'`.
Use `--isolated` to ignore config files completely.

## Recipes

**In CI**, fail on new problems only:

```sh
pymapper check --baseline .pymapper-baseline.json --output-format github
pymapper deps --check
```

**For a coding agent** after it edits files, check the changed files against the whole
program:

```sh
pymapper check --output-format json src/myapp/changed.py
pymapper impact myapp.module:function --output-format json     # before changing a function
```

**For architecture**, declare layers in `[[tool.pymapper.contracts]]` and run:

```sh
pymapper check --select PM3
```

**To clean up dead code** safely:

```sh
pymapper check --select PM4                      # high-confidence candidates only
pymapper check --select PM4 --min-confidence 0   # everything, with reasons, for a manual review
```

**To look for copy-paste:**

```sh
pymapper check --select PM5 --set 'analysis.dupes = true'
```

## Troubleshooting

**"no entry points found ... every definition will look unused".** You forced application
mode, or a package resolved to it, but pymapper found no `__main__`, scripts or tests. Use
`--mode library`, or declare your entry points under `[tool.pymapper.entrypoints]`.

**Something is reported as unused but a framework calls it.** Look at the reasons with
`--min-confidence 0`. Then do one of these:

- enable the framework's preset (`--preset fastapi`, ...);
- list the function in `entrypoints.names` (`"pkg.mod:func"`, or `"pkg.mod:*"` for a whole
  module);
- suppress the line with `# pymapper: ignore[PM401]`.

**Results look wrong for one sub-package.** Run `pymapper check --stats` and check the
`deadcode.mode` line for that package, and the project root pymapper chose.

**`error: unknown rule selector`, or "no rule is selected".** pymapper refuses to report
"All checks passed!" when nothing actually ran. Check the selector against `docs/rules.md`.
Some rules also need a switch: PM501 needs `analysis.dupes = true`, PM302–PM304 need
`[[contracts]]`, and PM6xx run under `pymapper deps --check`.

**A cold run is slow.** That happens with `--deps`, which loads the whole venv: about 3 s for
60k files. Warm runs reuse the cache. `--stats` shows where the time goes.
