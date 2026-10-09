# `pymapper deps`

`pymapper deps [PATHS]` lists the third-party distributions of the project's virtualenv, how
much of each the first-party code uses, and with `--score` a scorecard per distribution: size,
complexity, hotspots, maintainability, coupling and a letter grade. It is information for
humans and agents choosing, auditing or replacing dependencies; it never produces findings and
always exits 0 (2 only for usage and fatal errors, as `check`).

`pymapper deps --check [PATHS]` instead runs the dependency-hygiene rules PM601-PM604
(undeclared, unused, private and transitive-only dependencies) and reports findings exactly
like `check`; see [Hygiene](#hygiene---check).

```
$ pymapper deps --score
distribution  version  grade  score      loc  hot/kloc    mi   cc  deps  importers  api used  loc reached
...
PyYAML        6.0.3    C       61.2     5923       7.8  51.4  3.3     0         42     5/381  721/5923 (12%)
requests      2.34.2   C       69.8     6385       3.8  48.3  2.9     8          6     4/208  3689/6385 (58%)
rich          13.9.4   D       53.7    26607       4.3  46.0  2.7     6         10    18/586  15817/26607 (59%)
tenacity      8.5.0    B       82.5     2075       1.6  58.6  1.8     3         11     20/73  922/2075 (44%)
torch         2.13.0   D       48.9  1233764       4.5  41.9  3.0    92          1    3/7041  273667/1314561 (21%)
...

115 of 491 distributions used by first-party code (1803 first-party files); --all lists every one.
Scorecards: 1 computed, 490 cached.
```

(Rows taken from a real run on a project with 491 installed distributions; the column widths
of the real table follow its longest name.)

## Loading

The project is loaded exactly as `pymapper check --deps` loads it (the same project root,
configuration, import roots, walk and cache, see [`cli.md`](cli.md)): third-party code is
always loaded (`--deps` semantics; `--no-deps` is ignored with a warning), the stdlib only
with `--stdlib` / `include-stdlib`. Without a virtualenv the import roots warn and the list is
empty. The whole program is resolved (imports and calls), third-party code included.

Positional `PATHS` restrict which first-party files count as users: `pymapper deps src/api`
shows what the code under `src/api` uses. The whole project is loaded either way, and a path
covering the project root filters nothing. A path that does not exist is a usage error.

## Flags

| flag | meaning |
|------|---------|
| `--score` | add the scorecard columns (JSON: a `scorecard` object per distribution) |
| `--all` | list every distribution, not only those first-party code imports or calls |
| `--format concise\|json` | output format (`--output-format`, config `output-format`); `sarif` and `github` are usage errors |
| `--check` | run the dependency-hygiene rules PM601-PM604 instead of listing ([Hygiene](#hygiene---check)); all four formats |
| `--baseline FILE`, `--save-baseline FILE` | with `--check`: as for `check` |
| `--stats` | phase timings (`deps.usage`, `deps.score`, ...), counters and peak RSS on stderr |
| configuration flags | as for `check` (`--venv`, `--cache-dir`, `--no-cache`, `-j`, `--set`, `--config`, ...) |

## Usage columns

Computed on every run (never cached), from the first-party files in `PATHS`:

| column / JSON field | meaning |
|---------------------|---------|
| `importers` | first-party files importing the distribution (`import_rows`: their import rows) |
| `modules_used` (JSON) | distinct packages / modules of it imported directly |
| `api used` / `api_used` | distinct classes and functions of it referenced directly: targets of `from m import name` and of call edges from first-party code; with `--score`, out of its public API (`public_api`) |
| `call_sites` (JSON) | call edges from first-party code into it (one per call site and target) |
| `loc reached` / `loc_reached`, `loc` | lines of its code reachable from first-party code, out of all its loaded lines (stubs included) |

Reachability is a breadth-first search from every node (module and definition) of the
first-party files in scope over: call edges at least `likely` (unique-method-name guesses
would link first-party code to arbitrary libraries), import edges that are not type-only
(importing a module runs its module-level code) and edges from a module to its parent package
(importing `a.b` runs `a/__init__.py`). It is transitive across distributions: code of `urllib3`
that `requests` calls on behalf of first-party code counts for `urllib3`. Lines are exclusive
per node: a reached module counts its lines outside definitions, a reached class its body
outside methods, a reached function its own lines. Calls through attributes the resolver cannot
type (a value returned by a library call, a parameter) are not followed, so `loc reached` is a
lower bound.

A distribution that first-party code imports but whose code is not loaded (compiled modules
only, no `RECORD` file list) is listed with `- (code not loaded)`.

## Scorecard

Computed once per distribution and cached in `scores.pmc` in the cache directory, keyed like
the distribution's facts segment (normalized name, version, hash of its `RECORD`) plus the
scorecard version: a warm run computes none. Editable installs and distributions without a
`RECORD` are scored on every run. A scorecard depends only on the distribution's own code and
on how its imports resolved, never on the project or the configuration: thresholds are the
built-in defaults so scorecards compare across projects. Stub files (`.pyi`) are counted
(`stub_files`) but not measured.

| field | meaning |
|-------|---------|
| `files`, `stub_files` | loaded `.py` files / `.pyi` files |
| `loc`, `code_lines` | lines of the `.py` files, without docstrings |
| `classes`, `functions` | methods and nested functions included, `@overload` signatures excluded |
| `public_api` | classes and functions with a public dotted path (no package, module, class or enclosing function starting with `_`; not nested in a function; dunders excluded), plus those re-exported by a public module (`__all__`, `import x as x`, public names of an `__init__` without `__all__`) |
| `cc_mean`, `cc_max` | McCabe complexity per function (ruff C901 semantics) |
| `cognitive_mean`, `cognitive_max`, `nesting_max` | cognitive complexity per function, deepest block nesting |
| `hotspots`, `hotspot_density` (`hot/kloc`) | functions over at least one default function threshold (McCabe 10, cognitive 15, nesting 4, 60 code lines, 6 parameters), and their number per 1000 code lines |
| `mi_mean`, `mi_min` | maintainability index (0-100) of the functions, mean weighted by line span, and lowest |
| `internal_edges`, `fan_out_mean`, `fan_out_max` | import edges between its modules (type-only imports excluded), per module and at most |
| `cyclic_modules` | modules in an import cycle inside the distribution |
| `external_deps` (`deps`) | distinct other distributions and unattributed top-level packages it imports (stdlib and first-party code excluded), as resolved when the scorecard was computed |
| `score`, `grade` | see below; `null` / `-` for a distribution without functions |

### Grade

Four sub-scores in `[0, 1]`, with `clamp01(x) = min(max(x, 0), 1)`:

```
hotspots        H = 1 - clamp01(hotspot_density / 15)
maintainability M = clamp01((mi_mean - 25) / 40)
complexity      C = 1 - clamp01((cc_mean - 1.5) / 5)
coupling        K = 1 - 0.5 * clamp01(2 * cyclic_modules / files)
                    - 0.5 * clamp01((fan_out_mean - 1) / 4)

score = 100 * (0.30 H + 0.25 M + 0.25 C + 0.20 K)
grade = A if score >= 85, B >= 70, C >= 55, D >= 40, else F
```

The ranges were calibrated on a virtualenv of 491 distributions (477 with functions, 16.2M
lines of `.py` code): the median score is 70 and the grades split A 19%, B 32%, C 35%, D 12%,
F 2%. A grade says how a distribution's code compares with typical published Python code on
these four axes; it says nothing about correctness, security or maintenance activity. Size is
reported but not graded.

## JSON

One document with the conventions of `check`'s JSON (schema `version` 2, `tool`, a list, a
`summary`), one distribution per line, sorted by normalized name then version:

```
{"version":2,"tool":{"name":"pymapper","version":"0.3.0"},"distributions":[
{"name":"requests","version":"2.34.2","loaded":true,"used":true,"importers":6,"import_rows":6,"modules_used":2,"api_used":4,"call_sites":9,"loc_reached":3689,"loc":6385,"scorecard":{"grade":"C","score":69.77,"files":19,"stub_files":0,"loc":6385,"code_lines":5029,"classes":52,"functions":247,"public_api":208,"cc_mean":2.89,"cc_max":25,"cognitive_mean":3.55,"cognitive_max":40,"nesting_max":5,"hotspots":19,"hotspot_density":3.78,"mi_mean":48.3,"mi_min":26.77,"internal_edges":59,"fan_out_mean":3.11,"fan_out_max":12,"cyclic_modules":0,"external_deps":8}},
...
],"summary":{"distributions":491,"used":115,"listed":115,"first_party_files":1803,"scorecards_computed":1,"scorecards_cached":490}}
```

`scorecard` is present with `--score` (`null` for a distribution without loaded code), and so
are the `scorecards_*` counters. Reals have at most two decimals; fields are only ever added
within a version.

## Hygiene (`--check`)

```
$ pymapper deps --check
...
apps/aiml-ais-backend/aiml_ais_backend/listeners.py:10:6: PM601 `sqlalchemy` is provided by `SQLAlchemy`, which is not a declared dependency
...
apps/aiml-ais-backend/pyproject.toml:14:5: PM602 dependency `gunicorn` is declared but never imported
...
src/cogito-evaluations/cogito_evaluations/_legacy/datasets/hypothesis_verification/data.py:5:6: PM603 `numpy.random._generator` is private to `numpy`
...
Found 490 findings in 262 files (1803 files checked).
Not shown: 30 below min-confidence.
```

(Lines taken from a real run on a uv workspace of 15 project units; most of its PM601 findings
are imports of distributions that a workspace member pulls in, which `--extend-select PM604`
reports as transitive instead.)

`--check` runs the PM6xx rules ([`rules.md`](rules.md#dependencies)) with everything `check`
has: the formats (`concise`, `json`, `sarif`, `github`), the rule selection (`select`,
`--select PM604`, ...), `per-file-ignores`, inline suppressions, the confidence gate,
`--baseline` / `--save-baseline` / `baseline`, `--stats` and the exit codes (0 clean, 1
findings at or above the gate, 2 usage or fatal error). Only the PM6xx rules run: selecting
none of them is a usage error. `--check` conflicts with `--score` and `--all`. Positional
`PATHS` filter the report as for `check`: the whole project is analysed, and findings in Python
files and `pyproject.toml` files outside `PATHS` are dropped.

`--check` reads the same `baseline` config key as `check`. The two commands report disjoint
rules, so pointing both at one file makes `--save-baseline` from either overwrite the other's
entries: give `deps --check` its own file on the command line (`--baseline deps-baseline.txt`)
when `baseline` is set. Malformed `# pymapper: ignore[...]` comments in Python files are
warned about by `check` only (`--check` applies the valid codes silently); those in
`pyproject.toml` files are warned about by `--check`.

The project is loaded as `check` loads it (third-party code only with `--deps`): imports are
mapped to distributions through the venv's installed metadata either way, so `--check` needs
no third-party parse and costs about as much as a `check` with only cheap rules.

### Declarations

Every project unit (the project root and each directory below it with a `pyproject.toml`)
declares its own dependencies; a file is checked against the nearest unit that declares any
(a `pyproject.toml` with only tool configuration declares nothing). Declared means listed in:

| table | kind |
|-------|------|
| `[project] dependencies` | main (PM602 checks these) |
| `[project.optional-dependencies]` | optional |
| `[dependency-groups]` (PEP 735) | group |
| `[tool.poetry.dependencies]` | main (`optional = true`: optional; `python` ignored) |
| `[tool.poetry.dev-dependencies]`, `[tool.poetry.group.*.dependencies]` | group |
| `[tool.uv] dev-dependencies`, `[tool.pdm.dev-dependencies]` | group |

Requirement strings are PEP 508; names are compared PEP 503-normalised (`PyYAML` = `pyyaml`)
and extras are kept (`requests[socks]`). A unit whose `[project] dynamic` lists
`dependencies` is not checked by PM601, PM602 and PM604 (warning), unless it keeps them in
`[tool.poetry.dependencies]` (Poetry 2), which are then read as its main dependencies; one that lists
`optional-dependencies` is not checked by PM601 and PM604. Without any declaration, these rules
report nothing (warning); PM603 needs none. A declared dependency that is installed but
provides no importable name is a meta-package (`pydantic-ai` installs nothing and requires
`pydantic-ai-slim`, which provides `pydantic_ai`): what it requires directly (with the
requested extras) counts as declared, and it is used when one of those is imported (editable
installs and project units are never meta-packages). Imports of the project itself or of another unit
(a workspace member) are never undeclared, and a workspace member imported through its
first-party package counts as a used dependency of the importing unit. The development groups
(`[dependency-groups]`, `[tool.uv] dev-dependencies`, ...) of a uv workspace root (a
`pyproject.toml` with `[tool.uv.workspace]`) count as declared for the test code (as
`analysis.include-tests-in-metrics` defines it: `tests` packages, `test_*.py`, `conftest.py`,
...) of every unit below it: tools such as `pytest` are usually declared once there and
installed for the whole workspace. They do not count for production code, and the root's main
and optional dependencies never do: a member must declare what it imports at run time.

A `pyproject.toml` takes `# pymapper: ignore[PM602]` comments on the line of a requirement (a
`#` inside a TOML string is not a comment), and `per-file-ignores` patterns match it like any
file (`"pyproject.toml" = ["PM602"]`, `"libs/*/pyproject.toml" = [...]`).

### Mapping imports to distributions

Only absolute imports are mapped (relative imports are first-party). For each import row:

1. a module that resolves to loaded code: first-party code is never a dependency, stdlib code is
   ignored, a third-party group gives its distribution;
2. otherwise the top-level name: the stdlib table, then the distributions whose
   `top_level.txt` or `RECORD` (or editable-install mapping) list it; for a name several
   distributions share (namespace packages such as `google`), the distribution whose `RECORD`
   lists the imported module (`google/protobuf/__init__.py` -> `protobuf`), else any of them;
3. otherwise a built-in table of import names whose distribution is named differently (`yaml`
   -> `PyYAML`, `PIL` -> `Pillow`, `sklearn` -> `scikit-learn`, ... about 60 entries), else a guess
   from the name itself. A name several distributions provide (`cv2`: `opencv-python`,
   `opencv-python-headless`, `opencv-contrib-python`, ...; `faiss`; `Crypto`) is declared by any
   of them; a finding names the first.

The confidence of PM601, PM602 and PM604 says which step answered: 100 for installed metadata,
90 for the table, 60 for a guess (below the default gate of 80, so a project without a venv
only sees the table's answers by default). An import guarded by `except ImportError` is an
optional dependency: at most 50. A declared dependency that installs no importable name (a
command-line tool, a meta-package none of whose requirements is imported) is reported unused
with confidence 50; one that is not
installed is matched by its table entries and its guessed import name (`attrs`: `attr` and
`attrs`). PM601 and PM604 are reported once per import statement and distribution, at the
module token: `from yaml import (\n    safe_load,\n    dump,\n)` is one PM601 on its first
line, where one `# pymapper: ignore[PM601]` suppresses it.

PM604 (off by default) follows `Requires-Dist` from the installed declared dependencies,
transitively, honouring requested extras (`requests[socks]` pulls in `PySocks`; `requests`
alone does not), and names the declared dependency that pulls the import in. The `METADATA`
headers are read lazily, once per distribution reached, and only when an import is undeclared.

PM603 flags a component starting with `_` in the import path or the imported name of
third-party code (`pkg._internal`, `from pkg import _helper`), reported up to that component
(a private module once per statement, at the module; a private name at the name);
dunders (`__version__`) and the lone `_` (`from pkg import _`, the gettext idiom) are not
private. It needs the code to be known third-party (loaded, or attributed to an installed
distribution), never a guess.

## Cost

Measured numbers are in [`benchmarks.md`](benchmarks.md#deps-s61) (hygiene:
[`benchmarks.md`](benchmarks.md#dependency-hygiene-s62)). Scorecards cost one pass over
the definitions and module edges of the scored distributions (19 ms on one thread, 5 ms on 14,
for 491 distributions and 16.2M lines; 0.1 ms warm), usage one pass over the first-party imports and the call
edges plus a BFS over the call and import graphs (20 ms). Loading the venv and resolving calls
dominate, as for `check --deps`.

`--check` costs one pass over the first-party import rows (O(1) per row: 1 ms for 1,803 files,
7.6 ms for 901k import rows) plus one TOML parse per unit; it loads no third-party code unless
`--deps` is given, so a warm run takes about as long as a warm `check` (107-158 ms against
170 ms). PM604 adds the parallel reads of the `METADATA` of the distributions reachable from the
declared ones (16 ms for 394 files).
