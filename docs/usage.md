# Using pymapper

pymapper analyses a whole Python project without running it. It reads every file, resolves
imports and calls across modules, and then reports problems that single-file linters such as
ruff and ty cannot see. It has three parts:

| Part | What it does |
|---|---|
| `pymapper check` | finds problems and exits non-zero when it finds any, like ruff or pytest |
| query commands and `explain` | answer structural questions (who calls this, what breaks if I change it, how A reaches B) and explain one finding in depth |
| `pymapper-gui` | a native map of the codebase where you can see packages, modules, the links between them and quality metrics |

All examples below use the small demo project in `tests/corpus/demo`, a shop application
seeded with one deliberate problem per rule family. It is copied to a directory named
`sample_python`, and the commands are run from inside it unless the example says otherwise.
The output shown is real output from pymapper 0.4.0.

## Contents

- [Running pymapper](#running-pymapper)
- [Concepts you need first](#concepts-you-need-first)
- [pymapper check](#pymapper-check)
- [Output for people, agents and programs](#output-for-people-agents-and-programs)
- [pymapper explain](#pymapper-explain)
- [Suppressing and baselining](#suppressing-and-baselining)
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
`pymapper check services/pricing.py` reports on one file but still knows about every caller in
the whole project. When you filter, the output says so:

```
Filtered to services/pricing.py (1 of 23 first-party files); the whole project was analysed.
```

### Symbols

Symbols are written `package.module:Class.method`, for example
`services.auth:create_authenticator.authenticate`. The part before the colon is the module,
and the part after it is the path inside that module. Wherever a command asks for a symbol you
can give the full id, a dotted name, or just the last few segments, as long as that is
unambiguous. If it is ambiguous, pymapper lists the candidates and exits with code 2.

### Finding ids

Every finding has a short id such as `PM401-e3b2`: the rule code, then 4 hex digits. The id is
computed from the rule, the symbol and the stable parts of the message, never from line
numbers, so it stays the same from run to run and when code above it moves. It changes only
when the finding itself changes (the symbol is renamed or moved). You pass it to
`pymapper explain` to see everything pymapper knows about that finding.

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
  utils/decorators.py
     3:9   function  log_calls.wrapper      has no resolved callers (name appears in its own file) (confidence 10: name-in-same-file)  PM401-226f
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
tests, scripts, examples and private helpers are ignored. The demo declares
`[project.scripts] shop = "app:main"`, so it is an application. `check --stats` prints each
decision and the reason for it, and the `--llm` output states it in its header:

```
deadcode.mode .: application ([project.scripts] in pyproject.toml)
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
  middleware/pipeline.py
    21:5   function  format_receipt         never used                                    PM401-61ec
  models/base.py
    12:9   method    BaseModel.save         never used                                    PM403-7339
  models/order.py
    15:9   method    Order.archive          never used                                    PM403-d26a
  models/product.py
    10:9   method    Product.display_price  never used                                    PM403-5d6b
  models/user.py
    10:9   method    User.display_name      never used                                    PM403-0ad5
  repo/product_repo.py
     1:1   module    repo.product_repo      never imported                                PM404-d26e
  repo/user_repo.py
     9:5   function  delete_user            never used                                    PM401-5619
  services/auth.py
    11:5   function  make_validator         never used                                    PM401-5dad
    29:5   function  classify_token         never used                                    PM401-974d
    34:5   function  create_authenticator   never used                                    PM401-f0b1
  services/pricing.py
    53:5   function  legacy_discount        never used                                    PM401-145e
    57:7   class     PromoEngine            never used                                    PM402-d424
  services/user_service.py
    22:5   function  get_user_display       never used                                    PM401-f4f8
  utils/formatting.py
     5:5   function  format_currency        only used by unused code                      PM401-5fdd
     9:5   function  format_percentage      never used                                    PM401-04b6
  utils/legacy_helpers.py
     1:1   module    utils.legacy_helpers   never imported                                PM404-c008
  utils/validation.py
    15:5   function  validate_range         never used                                    PM401-c422

Unused imports  2
  middleware/pipeline.py
     6:25  User                                                                           PM405-f95d
  services/pricing.py
     1:8   json                                                                           PM405-4ed8

26 findings in 15 files (23 files checked) · architecture 2 · complexity 1 function · dead code 18 · unused imports 2
10 more below confidence 80 (use --min-confidence 0 to show them)
```

This is what the output shows you:

- **Sections by kind of work**, most important first: **Architecture** (import cycles and
  broken layering contracts: re-architect), Dependencies (only under `deps --check`),
  **Complexity** (refactor), Coupling, Duplication, **Dead code** (delete) and **Unused
  imports** (delete one line). Each header has a count; complexity counts functions, and
  duplication counts pairs. Sections with nothing in them are left out.
- **Files** inside each section, alphabetically, with their findings by line. Each line starts
  with `line:col`, so `services/pricing.py` + `6:5` is the place to open.
- **Short names.** The file is already the heading, so a finding names `calculate_price` or
  `Order.archive`, not the full qualified id.
- **One line per function for metrics.** `calculate_price` breaks four limits (McCabe 14 over
  10, cognitive complexity 32 over 15, nesting 5 over 4, 8 parameters over 7); they are merged
  into one line instead of four.
- **Cross-file findings once.** The cycle between `models.order` and `repo.order_repo` is
  listed once, under the file whose import closes it; the contract break is under the module
  that does the forbidden import.
- **Dead code** has a kind column (function, class, method, module) and a verdict: `never
  used` is the strongest case; `only used by unused code` (`format_currency`) means its only
  caller (`format_receipt`) is itself dead, so they go together.
- **The id at the end of each line** (`PM401-e3b2`) is what you pass to `pymapper explain`. A
  merged metric line shows the first finding's id and the other codes
  (`PM101-312c +PM102,PM103,PM105`).
- **The footer** gives the totals and a tally per section, and explains anything that is not
  shown:

| Footer line | Meaning |
|---|---|
| `26 findings in 15 files (23 files checked) · architecture 2 · ...` | the totals, and a tally per section |
| `10 more below confidence 80 (use --min-confidence 0 to show them)` | findings hidden by the confidence gate |
| `Not shown: 26 baselined.` / `... suppressed` | findings hidden by a baseline or by suppression comments |
| `Filtered to <paths> ...` | you passed paths, so only part of the project was reported |
| `17 rules ran, 8 skipped (...)` | printed when you choose rules explicitly; says which rules could not run and why |

On a terminal the output is coloured: section headers by family (architecture red, complexity
magenta, dead code yellow, ...), file paths bold, positions and ids dim. `--color never` turns
that off, `--color always` keeps it in a pipe (for `less -R`), and the `NO_COLOR` environment
variable is honoured. Ids are right-aligned to the width in `COLUMNS` (100 when unset).

### Grouping by file

When you are editing a few files, `--group-by file` puts each file first, with the kind of
problem as a column:

```
$ pymapper check --group-by file services/pricing.py
services/pricing.py
     1:8   unused import  json                                                            PM405-4ed8
     6:5   complexity     calculate_price  McCabe 14>10 · cognitive 32>15 · nesting 5>4 · params 8>7  PM101-312c +PM102,PM103,PM105
    53:5   dead code      function legacy_discount  never used                            PM401-145e
    57:7   dead code      class PromoEngine  never used                                   PM402-d424

7 findings in 1 file (1 file checked) · complexity 1 function · dead code 2 · unused imports 1
Filtered to services/pricing.py (1 of 23 first-party files); the whole project was analysed.
```

"7 findings" counts the four metric findings of `calculate_price` separately, though they are
shown as one line. Set `group-by = "file"` in the config to make it the default.

### Paths are relative to where you are

Paths are printed relative to the directory you run pymapper in, so you can click them or
paste them into your editor. Run from the directory above the project, the same findings read:

```
$ pymapper check sample_python --select PM3
Architecture  2
  sample_python/models/order.py
     2:29  imports forbidden repo.order_repo (contract: models are independent of repo)   PM303-e6ff
  sample_python/repo/order_repo.py
     1:26  import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo   PM301-e35f

2 findings in 2 files (23 files checked) · architecture 2
```

Module names and ids do not change. JSON and SARIF output keep paths relative to the project
root, because programs need a fixed base.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | no findings at or above the confidence gate |
| 1 | there are findings (not an error) |
| 2 | usage or fatal error: unknown rule selector, bad config, no runnable rule selected, etc. |

The exit codes are the same for every output format.

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
every rule with its exact definition, options and fix hint.

The output above already shows the architecture, complexity and dead-code families.
Duplication is off by default because it costs an extra pass; turn it on to find the
copy-pasted receipt code in the demo:

```
$ pymapper check --set 'analysis.dupes = true' --select PM5
Duplication  1 pair
  services/invoice.py:1  invoice_lines  ↔  services/pricing.py:39  receipt_lines  (12 lines) (confidence 98)  PM501-f258

2 findings in 2 files (23 files checked) · duplication 1 pair
```

A pair is one item with both locations (it is two findings, one per copy, in the totals and in
JSON). The confidence is just below 100 because two of the twelve lines differ (the function
name and the docstring).

### Dead-code wording and reasons

The wording tells you how strong the evidence is:

| Verdict | Meaning |
|---|---|
| `never used` / `never imported` | nothing refers to it at all: the strongest case |
| `only used by unused code` / `only imported by unused modules` | something refers to it, but that code is itself dead, so remove both |
| `has no resolved callers (...)` | no call was found, but the name appears somewhere; the brackets say where |

These are the reasons you will meet most often in the confidence suffix, the JSON `reasons`
array and `explain`:

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

`--stats` (on stderr) ends with the information you need when results look wrong:

```
  rules_run                    18
  rules_skipped                 4
  ...
  deadcode.units                1
  deadcode.library              0
  deadcode.application          1
  config_files                  1
  config_files_cached           0
  peak_rss                   10.3 MB
  skipped PM501: expensive, needs `analysis.dupes = true` (e.g. --set 'analysis.dupes = true')
  skipped PM601-PM603: dependency rules are run by `pymapper deps --check`, not `check`
  deadcode.mode .: application ([project.scripts] in pyproject.toml)
```

## Output for people, agents and programs

`check` (and every other command) can write its answer for three kinds of reader:

| Reader | Format | Flag |
|---|---|---|
| you, at a terminal | `human`: grouped, merged, coloured, every finding shown | (default) |
| a coding agent (an LLM) | `llm`: compact, prioritised, capped, with evidence, a fix hint and a check command per item | `--llm` |
| a program or CI | `json` (schema version 2), `sarif`, `github` annotations, or `concise` lines | `--output-format json` etc. |

### `--llm`: for agents

Agents pay for every token they read and cannot ask a follow-up question of the output, so
the llm format says up front what kind of run it was and what pymapper cannot see, puts the
most important items first, merges related findings, attaches the next step to every item and
stops after 50 items:

```
$ pymapper check --llm
# pymapper check · root /path/to/sample_python (reached as .) · paths relative to the working directory
Dead-code mode: application ([project.scripts] in pyproject.toml)
26 findings in 15 files, 19 items, all shown · 10 below confidence 80 not shown (--min-confidence 0 shows them) · exit 1 = findings, not an error
Not analysed: notebooks, C extensions, names used only via getattr or strings. Verify dead code before deleting it.
Rules not run: PM501: needs `analysis.dupes = true`; PM601-PM603: run by `pymapper deps --check`

## Architecture (2)
PM303-e6ff models/order.py:2 models.order imports forbidden module repo.order_repo (contract "models are independent of repo")
  fix: remove the import of repo.order_repo from models.order, or move the code that needs it out of models.order (contract "models are independent of repo") · verify: pymapper explain PM303-e6ff
PM301-e35f repo/order_repo.py:1 import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo
  fix: break one import of the cycle: import inside the function that needs it, or move the shared code into a lower-level module both can import · verify: pymapper explain PM301-e35f

## Complexity (1 function)
PM101-312c services/pricing.py:6-36 calculate_price: McCabe 14>10, cognitive 32>15, nesting 5>4, params 8>7 (PM101,PM102,PM103,PM105)
  fix: flatten calculate_price with guard clauses and extract nested blocks, to bring cognitive complexity from 32 to 15 or less · verify: pymapper callers services.pricing:calculate_price

## Dead code (14 items, 18 symbols)
PM401-61ec middleware/pipeline.py:21 format_receipt (function): never used (conf 100)
  also dead if it goes: utils/formatting.py: format_currency:5
  fix: delete format_receipt if nothing uses it by name, or declare how it is used ([entrypoints], a preset, __all__) · verify: grep -rnwE --exclude-dir=.venv 'format_receipt|format_currency' .
PM404-d26e repo/product_repo.py: module never imported (conf 100)
  fix: delete module repo.product_repo if nothing loads it by name, or declare it as an entry point · verify: grep -rnw --exclude-dir=.venv product_repo .
...
PM401-5dad services/auth.py: make_validator:11 (function), classify_token:29 (function), create_authenticator:34 (function): never used (conf 100)
  fix: delete make_validator if nothing uses it by name, or declare how it is used ([entrypoints], a preset, __all__) (likewise for the other 2) · verify: grep -rnwE --exclude-dir=.venv 'make_validator|classify_token|create_authenticator' .
...

## Unused imports (2)
PM405-f95d middleware/pipeline.py:6 User (conf 100)
  fix: delete the import of User, or add User to __all__ if it is part of the module's API · verify: grep -nw User middleware/pipeline.py
PM405-4ed8 services/pricing.py:1 json (conf 100)
  fix: delete the import of json, or add json to __all__ if it is part of the module's API · verify: grep -nw json services/pricing.py

Next: pymapper explain <id> for evidence and source · suppress with # pymapper: ignore[PMxxx] on the reported line
```

What it shows:

- **The header** is the context an agent would otherwise have to guess: the project root, the
  dead-code mode and why, the counts (26 findings merged into 19 items, all shown, 10 below
  the gate), that **exit 1 means findings, not a failure**, the blind spots ("Not analysed:
  notebooks, C extensions, ..."), and the rules that did not run.
- **Items, not lines.** The four metric findings of `calculate_price` are one item. The three
  dead functions of `services/auth.py` are one item. `format_receipt` and `format_currency`
  are one decision, a **dead chain**: delete the root (`format_receipt`) and
  `format_currency` goes with it ("also dead if it goes").
- **Priority.** Sections in the human order; inside a section, chains and whole modules come
  before single symbols, and complex functions by how far over the limit they are.
- **Evidence and next steps on every item:** the location (with the end line for spans,
  `6-36`), the confidence (`conf 100`), a `fix:` hint built from the real names and numbers,
  and a `verify:` command to run before acting (`pymapper explain <id>`, `pymapper callers`, or
  a ready `grep` line).
- **The cap.** At most `--limit` items (default 50). When items are cut, the last lines say how
  many and how to get them:

```
$ pymapper check --llm --limit 3
...
20 more findings not displayed (in 14 files). Use --limit 0 to show all, or --select <code> to narrow.
Next: pymapper explain <id> for evidence and source · suppress with # pymapper: ignore[PMxxx] on the reported line
```

`--limit 0` shows everything; `--select PM4` keeps one family. `--llm` cannot be combined with
`--output-format` (exit 2). The human format is never capped.

### JSON, SARIF, GitHub and concise: for programs

```sh
pymapper check --output-format json      # for scripts: one stable JSON document
pymapper check --output-format sarif     # for code-scanning tools
pymapper check --output-format github    # GitHub Actions annotations
pymapper check --output-format concise   # one `path:line:col: CODE message` line per finding
```

The JSON looks like this (version 2; `--select PM101,PM405` to keep it short):

```json
{"version":2,"tool":{"name":"pymapper","version":"0.4.0"},"root":"/path/to/sample_python","min_confidence":80,"findings":[
  {"code":"PM405","id":"PM405-f95d","name":"unused-import","severity":"warning","confidence":100,"path":"middleware/pipeline.py","line":6,"col":25,"end_line":6,"symbol":"middleware.pipeline:User","message":"`User` is imported but never used","reasons":["no-references"],"fix":"delete the import of `User`, or add `User` to `__all__` if it is part of the module's API"},
  {"code":"PM405","id":"PM405-4ed8","name":"unused-import","severity":"warning","confidence":100,"path":"services/pricing.py","line":1,"col":8,"end_line":1,"symbol":"services.pricing:json","message":"`json` is imported but never used","reasons":["no-references"],"fix":"delete the import of `json`, or add `json` to `__all__` if it is part of the module's API"},
  {"code":"PM101","id":"PM101-312c","name":"complex-function","severity":"warning","confidence":100,"path":"services/pricing.py","line":6,"col":5,"end_line":36,"symbol":"services.pricing:calculate_price","message":"`services.pricing:calculate_price` is too complex (McCabe 14 > 10)","value":14,"threshold":10,"fix":"extract branches of `services.pricing:calculate_price` into helpers, use a dispatch table or return early, to bring McCabe from 14 to 10 or less"}
],"summary":{"findings":3,"gated":3,"files":2,"files_checked":23,"suppressed":0,"baselined":0,"file_ignored":0,"below_confidence_hidden":0}}
```

JSON is one finding per entry, unmerged and uncapped, with paths relative to `root` (the
absolute project root). `id` is the same id the human and llm formats show, and `fix` the same
hint as `--llm`. Some fields appear only for certain rules:

| Field | Present on |
|---|---|
| `value`, `threshold` | metric rules (PM1xx, PM2xx) |
| `reasons` | dead-code findings with evidence |
| `cycle` | PM301: `size`, all `members` and a short `witness` path |
| `other` | PM501: the other copy's `path`, `line`, `end_line` and `symbol` |

SARIF and GitHub annotations carry the end line too, so a whole function is highlighted. The
`concise` format is the 0.3.0 default, kept byte for byte; use it for editors and scripts that
parse lines:

```
$ pymapper check --output-format concise
app.py:35:5: PM401 function `app:dead_endpoint` is never used
middleware/pipeline.py:6:25: PM405 `User` is imported but never used
middleware/pipeline.py:21:5: PM401 function `middleware.pipeline:format_receipt` is never used
...
Found 26 findings in 15 files (23 files checked).
Not shown: 10 below min-confidence.
```

## pymapper explain

`pymapper explain <id>` is the step between reading a finding and acting on it. It shows
everything pymapper knows about that one finding: the source, the evidence behind the
verdict, what else is affected, how to check it yourself and how to fix or silence it.

```
$ pymapper explain PM401-61ec
PM401-61ec · Dead code · unused-function: function middleware.pipeline:format_receipt is never used
  at middleware/pipeline.py:21:5 · middleware.pipeline:format_receipt (function) · confidence 100 (no-references)

  Source middleware/pipeline.py:21-24
    21 > def format_receipt(user, amount):
    22 |     name = format_name(user.name)
    23 |     price = format_currency(amount)
    24 |     return f"{name}: {price}"

  Evidence
    no-references: nothing refers to it at all
    liveness level 0/100 (confidence = 100 - level), from: no path from any root
    call sites: none (no resolved call, decorator or override edge reaches it)
    name `format_receipt`: appears nowhere else in the analysed Python files
    decorators: none
    mode: .: application ([project.scripts] in pyproject.toml)
    roots checked: __main__ 1, scripts 2 (e.g. app, app:main)

  Effect of deleting it
    also unused once it goes (1):
      utils.formatting:format_currency at utils/formatting.py:5 PM401-5fdd
    no import statement names it

  Blind spots
    not scanned in this repository: 0 notebooks (.ipynb), 0 compiled extensions (.so/.pyd); callbacks passed as values, getattr/string dispatch and calls from C are not edges

  Verify
    grep -rnw format_receipt . --exclude-dir=.venv --exclude-dir=.git --exclude-dir=.pymapper_cache
    pymapper callers middleware.pipeline:format_receipt

  Fix
    delete middleware.pipeline:format_receipt if nothing uses it by name, or declare how it is used ([entrypoints], a preset, __all__)

  Suppress
    add `# pymapper: ignore[PM401]` at the end of middleware/pipeline.py:21

Next: run the Verify command, then delete it with what Effect lists · `pymapper callers middleware.pipeline:format_receipt` to double-check
```

Reading it from the top: the **source** is re-read from disk, with `>` on the reported line.
The **evidence** says why the confidence is 100: no call, decorator or override reaches the
function, its name appears nowhere else, and the application roots (the `__main__` guard and
the `shop = "app:main"` script) do not lead to it. **Effect of deleting it** lists the chain:
`format_currency` becomes unused too. **Blind spots** tell you what pymapper could not look at
in this repository (here, nothing: no notebooks or compiled extensions). Run the **Verify**
commands before you delete.

The content depends on the family. For the complex function, `explain` shows every metric
against its limit and where the complexity comes from:

```
$ pymapper explain PM101-312c
PM101-312c · Complexity and size · complex-function: services.pricing:calculate_price is too complex (McCabe 14 > 10)
  also PM102-0ea0 · cognitive-complexity: services.pricing:calculate_price is hard to understand (cognitive complexity 32 > 15)
  also PM103-1b4f · deep-nesting: services.pricing:calculate_price nests blocks 5 levels deep (> 4)
  also PM105-7df2 · too-many-parameters: services.pricing:calculate_price takes 8 parameters (> 7)
  at services/pricing.py:6:5 · services.pricing:calculate_price (function) · confidence 100

  Metrics (value vs threshold)
    McCabe: 14 > 10 PM101 over
    cognitive: 32 > 15 PM102 over
    nesting: 5 > 4 PM103 over
    lines (excluding docstrings): 30 <= 100
    parameters (excluding self/cls): 8 > 7 PM105 over
    maintainability index: 44.5 >= 10.0

  Where the complexity comes from (services/pricing.py:6-36, approximate, from the source text)
    line 13: if at nesting 4 (cost ~5): if total > 1000:
    line 12: if at nesting 3 (cost ~4): if currency == "EUR":
    line 11: if at nesting 2 (cost ~3): if region == "EU":
    ...
    deepest nesting: 5 levels, lines 9 > 10 > 11 > 12 > 13

  Split points (largest nested blocks)
    lines 9-28 (20 lines): if block at nesting 0
    lines 10-26 (17 lines): if block at nesting 1
    lines 11-22 (12 lines): if block at nesting 2

  Refactor risk
    1 direct caller (0 possible); impact: 2 affected symbols in 1 file, 0 in tests
  ...
```

For the import cycle it lists every import between the members with its kind, says whether
the cycle is real at runtime, and points at the cheapest import to move:

```
$ pymapper explain PM301-e35f
...
  Import statements between members (2)
    models/order.py:2 models.order -> repo.order_repo (top-level): from repo.order_repo import next_order_id  # PM301 cycle + PM303 contract violation
    repo/order_repo.py:1 repo.order_repo -> models.order (top-level): from models.order import Order  # closes the cycle with models.order

  Runtime
    a real runtime cycle: the top-level imports alone close it, so importing any member first runs into a partly initialised module

  Cheapest edge to break (from the source text)
    models/order.py:2: `next_order_id` is used only inside functions or classes (lines 7): move this import into them
...
```

A contract break shows the contract and the other modules that import the same target, a
duplicate pair shows both copies and the lines that differ, and an unused import shows whether
it is a re-export. Dependency findings (`deps --check`) are not covered yet.

Things to know:

- **Give a symbol instead of an id** to see every finding on it, below the gate included, after
  a summary of its callers, callees and importers:
  `pymapper explain services.pricing:calculate_price`.
- **Use the same options as the `check` run.** `explain` runs the analysis again (cheap with a
  warm cache). The PM501 id above is found only with `--set 'analysis.dupes = true'` again.
  Findings below the confidence gate are explained without changing `--min-confidence`, with a
  note that `check` hides them.
- **An unknown id exits 2** with `no finding has this id (the code changed since, or other
  options hide it ...)`: re-run `check` for current ids.
- `--llm` gives the same content with Markdown headings and no indentation.

## Suppressing and baselining

You can silence a finding on its own line. The line is the `def`/`class` line, the import
line, or line 1 for module findings (`explain` prints the exact line under "Suppress"):

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

```
$ pymapper check --baseline .pymapper-baseline.json
All checks passed! (23 files checked)
10 more below confidence 80 (use --min-confidence 0 to show them)
Not shown: 26 baselined.
```

## Query commands

These commands answer questions instead of looking for problems. Their default output has one
fact per line, starting with a clickable `path:line:col` and ending with how sure the call is
(`[certain]`, `[likely]`, `[possible]`), then one summary line. Every query command also
accepts `--llm` (grouped, with the query's blind spots and a next step, for agents) and
`--output-format json`. Long answers are capped at 200 rows (50 with `--llm`); the summary line
says when rows were cut, and `--limit 0` shows them all.

### summary: a codebase at a glance

```
$ pymapper summary
first-party: 23 files, 362 lines, 5 packages, 18 modules, 5 classes, 51 functions
imports: 34 rows, 0 unresolved (0.0%), 29 module edges
calls: 106 sites: 34.0% certain, 0.0% likely, 13.2% possible, 52.8% unresolved; 50 caller-callee pairs
import cycles: 1 (largest: 2 modules)
services/pricing.py:6:5: cognitive services.pricing:calculate_price 32
services/auth.py:34:5: cognitive services.auth:create_authenticator 8
services/auth.py:1:5: cognitive services.auth:require_auth 6
...
services/pricing.py:6:5: cyclomatic services.pricing:calculate_price 14
...
models/base.py:2:9: most called models.base:BaseModel.validate (5 callers)
...
models/base.py: most imported models.base (4 importers)
...
```

- **Line 1:** the size of your code.
- **Line 2:** how many imports pymapper could resolve. "Unresolved" usually means a missing
  dependency or a dynamic import.
- **Line 3:** how much of the call graph pymapper understands. "Unresolved" calls are mostly
  calls into third-party or builtin code, or calls on values whose type pymapper cannot infer.
  Run with `--deps` to resolve more of them.
- **Line 4:** the number of import cycles and the size of the largest.
- **The rest:** the worst hotspots by cognitive and cyclomatic complexity, the most called
  functions and the most imported modules, ready to click.

With `--llm`, the rankings are merged per function and "most called" counts only real
(certain or likely) callers. `BaseModel.validate` drops from first place, because three of its
five callers are guesses from the method name:

```
$ pymapper summary --llm
...
## Most called (distinct certain/likely callers; possible callers not counted)
utils/formatting.py:1 format_name: 4 callers
services/user_service.py:9 create_user: 3 callers
utils/decorators.py:1 log_calls: 3 callers
cycles.py:1 ping: 2 callers
models/base.py:2 BaseModel.validate: 2 callers (+3 possible)
...
```

### callers / callees: one step up or down the call graph

```
$ pymapper callers services.user_service:create_user
app.py:14:13: app:list_users [certain]
app.py:26:5: app:main [certain]
middleware/pipeline.py:17:12: middleware.pipeline:run_pipeline [certain]
3 callers of services.user_service:create_user (function, services/user_service.py:9)

$ pymapper callees app:main
app.py:28:5: cycles:ping [certain] def cycles.py:1:5
app.py:27:5: middleware.pipeline:run_pipeline [certain] def middleware/pipeline.py:10:5
...
app.py:26:5: services.user_service:create_user [certain] def services/user_service.py:9:5
10 callees of app:main (function, app.py:24)
```

Each line is a call site: where the call happens, the other function involved, and how sure
pymapper is. `callees` also gives the location of the definition (`def path:line:col`). The
`--llm` version groups the callers by file and says what the call graph cannot contain:

```
$ pymapper callers services.user_service:create_user --llm
# pymapper callers services.user_service:create_user
target: services.user_service:create_user (function, services/user_service.py:9) · root: . · 3 callers in 2 files (0 possible)
Confidence: certain/likely = resolved call; possible = guessed from a unique method name; dispatch = base method to an override.
Not call edges (results may be incomplete): functions passed as values (callbacks, registries), getattr/string dispatch, calls from C extensions and notebooks, and unresolved calls (52.8% of call sites).

## app.py (app)
list_users [certain] at 14:13
main [certain] at 26:5

## middleware/pipeline.py (middleware.pipeline)
run_pipeline [certain] at 17:12

Next: `pymapper impact services.user_service:create_user` for everything that depends on it transitively · `--depth N` for callers of callers
```

### impact: what can break if I change this

```
$ pymapper impact utils.validation:validate_email
middleware/pipeline.py:13:5: middleware.pipeline:run_pipeline [certain]
services/user_service.py:11:5: services.user_service:create_user [certain] +4 sites
app.py:27:5: app:main [certain] depth 2 via middleware.pipeline:run_pipeline
app.py:14:13: app:list_users [certain] depth 2 via services.user_service:create_user
app.py:40:5: app (module level) [certain] depth 3 via app:main
middleware/pipeline.py:1:30: middleware.pipeline [importer] imports utils.validation +2 rows
services/user_service.py:1:30: services.user_service [importer] imports utils.validation
app.py:3:33: app [importer] imports middleware.pipeline
impact of utils.validation:validate_email (function, utils/validation.py:1): 5 affected (4 symbols, 1 module level), 0 in tests, 3 files; 3 importing modules; most affected: app 3, middleware.pipeline 1, services.user_service 1
```

This walks the call graph backwards as far as it goes. Direct callers come first; `depth 2 via
X` means the symbol reaches your function through X, and `(module level)` is code that runs on
import. `[importer]` rows are modules that import the target's module. The last line is the
blast radius, including how much of it is test code. This is the command to run before a
refactor. With `--llm`, the affected symbols are grouped by file and the importing modules
are collapsed to a count and the first few, so a widely imported module does not bury the
callers.

### path: how does A reach B

```
$ pymapper path app:main repo.user_repo:save_user
app.py:26:5: app:main -> services.user_service:create_user [certain]
services/user_service.py:18:5: services.user_service:create_user -> repo.user_repo:save_user [certain]
call path from app:main (function, app.py:24) to repo.user_repo:save_user (function, repo/user_repo.py:1): 2 edges, least sure certain
```

This gives the shortest call chain, one hop per line. "Least sure" is the weakest link in the
chain: `possible` means at least one hop is a guess. `path` exits 1 when there is no path.

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
repo/order_repo.py:1:26: import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo
1 import cycle

$ pymapper cycles --calls --symbol cycles:ping
cycles.py:2:5: call cycle (2 functions): cycles:ping -> cycles:pong -> cycles:ping [certain]
1 call cycle through cycles:ping (function, cycles.py:1)
```

The size is the number of modules involved in the whole cycle. The path shown is one short
example loop you can break; the JSON output lists every member. `--llm` adds every edge with
its kind and says whether the cycle is eager (every import runs at import time) or closed only
by deferred imports, which is usually harmless:

```
$ pymapper cycles --llm
...
## Cycle 1: 2 modules, eager (every edge runs on import)
repo.order_repo -> models.order -> repo.order_repo
repo.order_repo -> models.order (top-level) at repo/order_repo.py:1:26
models.order -> repo.order_repo (top-level) at models/order.py:2:29
...
```

## pymapper deps

`pymapper deps` looks at the third-party packages in your virtualenv. It finds the venv from
`.venv`, `$VIRTUAL_ENV` or `--venv DIR`. The demo has no third-party code, so for this section
it gets a `.venv` with httpx, PyYAML, requests and rich installed, a `clients/http.py` module
that uses them, and `dependencies = ["httpx", "requests", "pydantic"]` in its
`pyproject.toml`.

### Which packages you use, and how much of each

```
$ pymapper deps
distribution  version  importers  api used  loc reached
httpx         0.28.1           1         1  2905/8828 (33%)
PyYAML        6.0.3            1         1  656/5923 (11%)
requests      2.34.2           1         3  3586/6385 (56%)
rich          15.0.0           1         2  14859/38515 (39%)

4 of 16 distributions used by first-party code (25 first-party files); --all lists every one.
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
httpx         0.28.1   C       65.8   8828       6.9  50.5  2.2    13          1       1/2  2905/8828 (33%)
PyYAML        6.0.3    C       61.2   5923       7.8  51.4  3.3     0          1     1/381  656/5923 (11%)
requests      2.34.2   C       69.5   6385       3.8  48.3  2.9     8          1     3/208  3586/6385 (56%)
rich          15.0.0   C       57.9  38515       3.0  45.6  2.8     5          1     2/594  14859/38515 (39%)

4 of 16 distributions used by first-party code (25 first-party files); --all lists every one.
Scorecards: 16 computed, 0 cached.
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
`docs/deps.md`. Scores never affect `check`'s exit code. `deps --score --llm` gives one line
per package, the one you reach most code of first:

```
$ pymapper deps --score --llm
...
rich 15.0.0: grade C (57.9) · importers 1 · names used 2 (public api 594) · loc reached 14859/38515 (39%)
requests 2.34.2: grade C (69.5) · importers 1 · names used 3 (public api 208) · loc reached 3586/6385 (56%)
...
```

### Dependency hygiene

```
$ pymapper deps --check --min-confidence 0
Dependencies  3
  clients/http.py
     3:8   yaml is provided by PyYAML, which is not a declared dependency                 PM601-8c8d
     4:6   rich.console is provided by rich, which is not a declared dependency           PM601-8d8a
  pyproject.toml
     4:38  dependency pydantic is declared but never imported (confidence 60)             PM602-85d5

3 findings in 2 files (25 files checked) · dependencies 3
```

This compares what you import with what `pyproject.toml` declares. It finds undeclared
imports (PM601), unused dependencies (PM602), imports of a package's private internals
(PM603) and, if enabled, imports of packages you only get transitively (PM604). The unused
`pydantic` is only at confidence 60, so a plain `deps --check` hides it: a package can be used
without being imported (a plugin, a CLI tool, a runtime requirement of another package). It
uses the same formats (`--llm` included), suppressions, baselines and exit codes as `check`.

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
output-format = "human"       # human | llm | concise | json | sarif | github
group-by = "category"         # human output: category | file
color = "auto"                # human output: auto | always | never
limit = 50                    # llm output: items shown, 0 = all

[tool.pymapper.rules]
ignore = ["PM105"]
per-file-ignores = { "scripts/**" = ["PM1"] }

[tool.pymapper.thresholds]
max-mccabe = 12

[tool.pymapper.entrypoints]
names = ["myapp.plugins.tools:*"]   # functions a framework loads by name from config files

[[tool.pymapper.contracts]]
name = "models are independent of repo"
kind = "forbidden"            # also: layers, independence
source = ["models"]
forbidden = ["repo"]
```

The contract above is the one the demo declares, and the one its PM303 finding breaks. Any
option can be set for one run with `--set`, e.g. `--set 'thresholds.max-mccabe = 12'`. Use
`--isolated` to ignore config files completely.

## Recipes

**In CI**, fail on new problems only:

```sh
pymapper check --baseline .pymapper-baseline.json --output-format github
pymapper deps --check
```

**For a coding agent** working on a codebase:

```sh
pymapper check --llm                         # read the findings, most important first
pymapper explain PM401-61ec                  # before acting on one: evidence, effect, verify commands
pymapper check --llm --select PM4 --limit 0  # when the trailer says items were cut
pymapper impact myapp.module:function --llm  # before changing a function
pymapper check --llm src/myapp/changed.py    # after editing, against the whole program
```

**For a script** that processes findings, use `--output-format json` (stable fields, ids,
uncapped), or `--output-format concise` for line-based tools.

**For architecture**, declare layers in `[[tool.pymapper.contracts]]` and run:

```sh
pymapper check --select PM3
```

**To clean up dead code** safely:

```sh
pymapper check --select PM4                      # high-confidence candidates only
pymapper check --select PM4 --min-confidence 0   # everything, with reasons, for a manual review
pymapper explain <id>                            # what else goes with it, and how to verify
```

**To look for copy-paste:**

```sh
pymapper check --select PM5 --set 'analysis.dupes = true'
```

## Troubleshooting

**"no entry points found ... every definition will look unused".** You forced application
mode, or a package resolved to it, but pymapper found no `__main__`, scripts or tests. Use
`--mode library`, or declare your entry points under `[tool.pymapper.entrypoints]`.

**Something is reported as unused but a framework calls it.** Look at the evidence with
`pymapper explain <id>`, or at the reasons with `--min-confidence 0`. Then do one of these:

- enable the framework's preset (`--preset fastapi`, ...);
- list the function in `entrypoints.names` (`"pkg.mod:func"`, or `"pkg.mod:*"` for a whole
  module);
- suppress the line with `# pymapper: ignore[PM401]`.

**`explain` says "no finding has this id".** The code changed since the id was printed, or
the `check` run used options `explain` did not get (`--set 'analysis.dupes = true'`,
`--select`, `--mode`, ...). Pass the same options, or run `check` again for current ids.

**Results look wrong for one sub-package.** Run `pymapper check --stats` and check the
`deadcode.mode` line for that package, and the project root pymapper chose (the `--llm` header
shows both).

**`error: unknown rule selector`, or "no rule is selected".** pymapper refuses to report
"All checks passed!" when nothing actually ran. Check the selector against `docs/rules.md`.
Some rules also need a switch: PM501 needs `analysis.dupes = true`, PM302–PM304 need
`[[contracts]]`, and PM6xx run under `pymapper deps --check`.

**Colour codes in a file or a pipe.** Colour is only on by default on a terminal (and never
when `NO_COLOR` is set); `--color always` or `color = "always"` forces it everywhere. Remove
that, or pass `--color never`.

**A cold run is slow.** That happens with `--deps`, which loads the whole venv: about 3 s for
60k files. Warm runs reuse the cache. `--stats` shows where the time goes.
