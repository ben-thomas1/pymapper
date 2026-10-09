<!-- Generated from crates/pymapper-core/src/rules by rules::render_markdown. Do not edit; regenerate with `PYMAPPER_BLESS=1 cargo test -p pymapper-core rules::tests::docs_up_to_date`. -->

# pymapper rules

Every finding belongs to a rule with a code `PMxxx`. Codes are grouped by category in ranges of
one hundred.

## Selecting rules

Rules are chosen in `[rules]` (or with `--select`, `--extend-select`, `--ignore`) by code
prefix: `ALL`, `PM`, `PM1`, `PM10`, `PM101`. Without `select`, the rules marked "on" below
run. When a rule matches both a select and an ignore selector, the more specific selector wins;
on a tie, ignore wins. `per-file-ignores` disables rules in files matching a glob. A selector
that matches no rule (`PM999`, anywhere in the rule options) is a usage error (exit 2) with a
"did you mean" suggestion when one is close.

Expensive rules also need their `[analysis]` toggle ("needs ..." below): turning the toggle on
enables them unless they are ignored. `pymapper check` does not run the dependency rules (PM6xx,
they belong to `pymapper deps`) or the contract rules PM302-PM304 without `[[contracts]]`. If
none of the selected rules can run, `check` fails with exit 2 and says why; if some can, it warns
about the skipped rules selected by code or category (`PM501`, `PM6`) and, whenever rules were
selected explicitly (`ALL` included), its footer says how many rules ran and which were skipped
("25 rules ran, 8 skipped (PM501: needs ...; ...)"). Analyses whose rules
are all disabled do not run at all.

## Thresholds

Metric rules read a limit from `[thresholds]` and report values strictly above a `max-*` limit
(strictly below a `min-*` limit).

## Test code

By default the complexity (PM1xx), coupling (PM2xx) and duplication (PM5xx) rules skip test code:
files whose dotted module name has a `tests`, `test` or `testing` component (a directory so named
below the import root, at any depth) or a private `_tests` / `_testing` one (test-support packages
shipped inside a library, such as `pandas/_testing`), or whose name is `test_*.py`, `*_test.py` or
`conftest.py`.
Long test functions, large test classes and repeated test setup are normal and would drown the
findings about the code under test. Set `analysis.include-tests-in-metrics = true` to report them
too. Dead code (PM4xx) and import cycles / contracts (PM3xx) always cover tests.

## Symbols

A finding's `symbol` is the stable qualified id of what it is about: `pkg.mod` for a module,
`pkg.mod:Class.method` for a definition. Baselines key on it. Same-named siblings (a property
and its setter, conditional definitions) are told apart by a `#N` suffix in source order: the
first is `name`, the next `name#2`, and so on. In a group with `@overload` signatures, the
implementation (the last definition of the group that is not an `@overload`) is the plain `name`
and the signatures are `name#2`, `name#3`, ... in source order, so adding an overload does not
change the id metric findings and baselines use (overload signatures themselves are never
reported by the metric rules). Unused imports (PM405) are shown as `pkg.mod:<bound name>` in
JSON and SARIF output.

## Confidence

Every finding has a confidence from 0 to 100. Measured facts (metrics, contract violations)
always have 100; heuristic rules (dead code, dependency mapping) score each finding as described
per rule. `pymapper check` reports findings with confidence at or above `min-confidence`
(default 80, `--confidence`) and exits 1 if there are any.
Dead-code findings below 100 name the evidence that lowered the confidence: concise output
appends `(confidence 50: name-elsewhere)`, JSON a `reasons` array (absent when there are none).

## Suppressing findings

Add `# pymapper: ignore[CODES]` to the line a finding is reported on (the `def` / `class` line,
the import line, or line 1 for module-level findings). Codes are comma-separated selectors, so
`# pymapper: ignore[PM101, PM4]` suppresses PM101 and every dead-code rule on that line; a bare
`# pymapper: ignore` suppresses every rule. `pymapper check` warns on stderr
(`pymapper: warning: path:line: ...`) about codes that are not pymapper selectors (`E501`), wrong
case (`pm101`, with a suggestion), codes that match no rule (`PM999`) and empty lists
(`ignore[]`); such codes suppress nothing, and the warnings never change the exit code. Only
files in the reported paths are diagnosed.

Ruff's `# noqa` is honoured for unused imports only: a bare `# noqa` or a `# noqa` listing F401
(`# noqa: F401`, `# noqa:F401,E501`, any case) suppresses PM405 on its line, or on every name of
a multi-line import when it is on the statement's first line. Other codes suppress nothing.

## Categories

| range | category | rules |
|-------|----------|-------|
| PM1xx | [Complexity and size](#complexity-and-size) | 7 |
| PM2xx | [Coupling](#coupling) | 4 |
| PM3xx | [Import cycles and layering](#import-cycles-and-layering) | 4 |
| PM4xx | [Dead code](#dead-code) | 5 |
| PM5xx | [Duplication](#duplication) | 1 |
| PM6xx | [Dependencies](#dependencies) | 4 |

Codes PM000-PM099 and PM700-PM999 are unassigned.

## All rules

| code | name | severity | default | cost | confidence | summary |
|------|------|----------|---------|------|------------|---------|
| [PM101](#pm101-complex-function) | `complex-function` | warning | on | O(n) | 100 | Function with a McCabe cyclomatic complexity above `max-mccabe` |
| [PM102](#pm102-cognitive-complexity) | `cognitive-complexity` | warning | on | O(n) | 100 | Function with a cognitive complexity above `max-cognitive` |
| [PM103](#pm103-deep-nesting) | `deep-nesting` | warning | on | O(n) | 100 | Function whose blocks nest deeper than `max-nesting` |
| [PM104](#pm104-long-function) | `long-function` | warning | on | O(n) | 100 | Function longer than `max-function-loc` lines, docstrings excluded |
| [PM105](#pm105-too-many-parameters) | `too-many-parameters` | warning | on | O(n) | 100 | Function with more than `max-params` parameters |
| [PM106](#pm106-low-maintainability) | `low-maintainability` | info | off | O(n) | 100 | Function with a maintainability index below `min-maintainability` |
| [PM107](#pm107-long-module) | `long-module` | info | on | O(n) | 100 | Module longer than `max-module-loc` lines, docstrings excluded |
| [PM201](#pm201-high-fan-out) | `high-fan-out` | warning | on | O(V+E) | 100 | Module importing more than `max-fan-out` distinct first-party modules |
| [PM202](#pm202-main-sequence-distance) | `main-sequence-distance` | info | off | O(V+E) | 100 | Coupled module far from the main sequence (distance above `max-distance`) |
| [PM203](#pm203-god-module) | `god-module` | warning | on | O(V+E) | 100 | Module that is both large (`max-module-symbols`) and highly coupled (`max-module-coupling`) |
| [PM204](#pm204-god-class) | `god-class` | warning | on | O(V+E) | 100 | Class that is both complex (`max-class-wmc`) and uses many other classes (`max-class-fan-out`) |
| [PM301](#pm301-import-cycle) | `import-cycle` | warning | on | O(V+E) | 100 | Modules that import each other, directly or indirectly |
| [PM302](#pm302-layer-violation) | `layer-violation` | error | on | O(V+E) | 100 | Import from a lower layer into a higher one, breaking a `layers` contract |
| [PM303](#pm303-forbidden-import) | `forbidden-import` | error | on | O(V+E) | 100 | Import breaking a `forbidden` contract |
| [PM304](#pm304-independence-violation) | `independence-violation` | error | on | O(V+E) | 100 | Import between modules of an `independence` contract |
| [PM401](#pm401-unused-function) | `unused-function` | warning | on | O(V+E) | scored | Module-level or nested function that is never used |
| [PM402](#pm402-unused-class) | `unused-class` | warning | on | O(V+E) | scored | Class that is never instantiated, subclassed or referenced |
| [PM403](#pm403-unused-method) | `unused-method` | warning | on | O(V+E) | scored | Method of a live class that is never called |
| [PM404](#pm404-unused-module) | `unused-module` | warning | on | O(V+E) | scored | First-party module that nothing imports and that is not an entry point |
| [PM405](#pm405-unused-import) | `unused-import` | warning | on | O(V+E) | scored | Import that neither its module nor any importer of that module uses |
| [PM501](#pm501-duplicate-code) | `duplicate-code` | info | needs `analysis.dupes` | expensive | scored | Block of code duplicated elsewhere (token-based clone) |
| [PM601](#pm601-undeclared-dependency) | `undeclared-dependency` | error | on | O(n) | scored | Import of a third-party distribution not declared in `pyproject.toml` |
| [PM602](#pm602-unused-dependency) | `unused-dependency` | warning | on | O(n) | scored | Declared dependency that no first-party module imports |
| [PM603](#pm603-private-import) | `private-import` | warning | on | O(n) | 100 | Import of a private module or name of a third-party package |
| [PM604](#pm604-transitive-dependency) | `transitive-dependency` | warning | off | O(n) | scored | Import of a distribution installed only as a dependency of a dependency |

## Complexity and size

### PM101 complex-function

Function with a McCabe cyclomatic complexity above `max-mccabe`.

- Message: `<symbol> is too complex (McCabe <int> > <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-mccabe` (reported above; default 10)
- Confidence: always 100

McCabe complexity counts the independent paths through a function, exactly as ruff's C901 / PyCQA mccabe: 1, plus one per `if`, `elif`, `for`, `while`, `except` handler, `try`-`else`, nested function and `match` case (minus one when the last case is an irrefutable, unguarded pattern). Boolean operators, conditional expressions, comprehensions, `with` and `assert` are not counted. High values mean many paths to test and reason about.

Fix: extract branches into well-named helpers, replace `if`/`elif` chains with a dispatch table, or return early.

Suppress on a line with `# pymapper: ignore[PM101]`.

### PM102 cognitive-complexity

Function with a cognitive complexity above `max-cognitive`.

- Message: `<symbol> is hard to understand (cognitive complexity <int> > <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-cognitive` (reported above; default 15)
- Confidence: always 100

Cognitive complexity (SonarSource) weights control flow by how deeply it is nested: `if`, conditional expressions, `match`, `for`, `while` and `except` cost one plus the current nesting level; `elif` and `else` cost one; each run of like boolean operators costs one; direct recursion costs one. Nested functions and lambdas count toward their parent.

Fix: flatten nesting with guard clauses, extract nested blocks into functions, and simplify boolean conditions.

Suppress on a line with `# pymapper: ignore[PM102]`.

### PM103 deep-nesting

Function whose blocks nest deeper than `max-nesting`.

- Message: `<symbol> nests blocks <int> levels deep (> <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-nesting` (reported above; default 4)
- Confidence: always 100

The maximum depth of nested compound statements (`if`, `for`, `while`, `try`, `with`, `match`) inside a function body. Deeply nested code is hard to follow and usually mixes several responsibilities.

Fix: return or `continue` early, and move inner blocks into helper functions.

Suppress on a line with `# pymapper: ignore[PM103]`.

### PM104 long-function

Function longer than `max-function-loc` lines, docstrings excluded.

- Message: `<symbol> is <int> lines long without docstrings (> <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-function-loc` (reported above; default 100)
- Confidence: always 100

Lines of the function's inclusive span, from its first decorator or `def` to its last line, minus the lines of its docstring and of the docstrings of functions and classes nested in it (a long numpydoc docstring is documentation, not code). Blank lines and comments count. A docstring is the first statement of a body when it is a plain string literal (not an f-string or bytes) on its own line(s). Long functions tend to do several things.

Fix: split the function along its steps.

Suppress on a line with `# pymapper: ignore[PM104]`.

### PM105 too-many-parameters

Function with more than `max-params` parameters.

- Message: `<symbol> takes <int> parameters (> <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-params` (reported above; default 7)
- Confidence: always 100

Counts every parameter (positional-only, regular, `*args`, keyword-only, `**kwargs`) except a leading `self` / `cls` of a method. Many parameters make calls hard to read and suggest the function does too much.

Fix: group related parameters into a dataclass, or split the function.

Suppress on a line with `# pymapper: ignore[PM105]`.

### PM106 low-maintainability

Function with a maintainability index below `min-maintainability`.

- Message: `<symbol> has a low maintainability index (<real> < <real>)`
- Severity: info
- Enabled by default: no (enable with `select` / `extend-select`)
- Cost: O(n)
- Threshold: `thresholds.min-maintainability` (reported below; default 10)
- Confidence: always 100

Maintainability index on a 0-100 scale: `clamp((171 - 5.2 ln V - 0.23 CC - 16.2 ln LOC) * 100 / 171, 0, 100)`, where V is the Halstead volume, CC the McCabe complexity and LOC the line count. It blends size and complexity into one number and is best used to rank code, which is why the rule is off by default.

Fix: reduce the function's size and complexity (see PM101 and PM104).

Suppress on a line with `# pymapper: ignore[PM106]`.

### PM107 long-module

Module longer than `max-module-loc` lines, docstrings excluded.

- Message: `module <symbol> is <int> lines long without docstrings (> <int>)`
- Severity: info
- Enabled by default: yes
- Cost: O(n)
- Threshold: `thresholds.max-module-loc` (reported above; default 1000)
- Confidence: always 100

Lines of the module file minus the lines of every docstring in it (the module's, and those of its classes and functions, as for PM104). Blank lines and comments count. Very long modules are hard to navigate and usually hold several unrelated concerns.

Fix: split the module into a package of focused modules.

Suppress on a line with `# pymapper: ignore[PM107]`.


## Coupling

### PM201 high-fan-out

Module importing more than `max-fan-out` distinct first-party modules.

- Message: `module <symbol> imports <int> first-party modules (> <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Threshold: `thresholds.max-fan-out` (reported above; default 30)
- Confidence: always 100

Efferent coupling: the number of distinct first-party modules a module imports (at any scope, `if TYPE_CHECKING:` imports excluded). A module that depends on many others breaks whenever any of them changes and is hard to test in isolation.

Fix: move unrelated responsibilities into separate modules, or depend on a narrower facade.

Suppress on a line with `# pymapper: ignore[PM201]`.

### PM202 main-sequence-distance

Coupled module far from the main sequence (distance above `max-distance`).

- Message: `module <symbol> is far from the main sequence (D <real> > <real>; abstractness <real>, instability <real>)`
- Severity: info
- Enabled by default: no (enable with `select` / `extend-select`)
- Cost: O(V+E)
- Threshold: `thresholds.max-distance` (reported above; default 0.7)
- Threshold: `thresholds.distance-min-coupling` (parameter; default 10)
- Confidence: always 100

Robert C. Martin's package metrics, per module. Afferent coupling Ca is the number of distinct first-party modules importing the module, efferent coupling Ce the number it imports (`if TYPE_CHECKING:` imports excluded). Instability is `I = Ce / (Ca + Ce)`; abstractness `A` is the share of its classes that are abstract (an `ABC` / `Protocol` base, an `ABCMeta` metaclass, or an `@abstractmethod`). The distance from the main sequence is `D = |A + I - 1|`. Near `A = 0, I = 0` lies the zone of pain (concrete code many modules depend on: every change ripples); near `A = 1, I = 1` the zone of uselessness (abstractions nobody uses). Only modules with `Ca + Ce` of at least `distance-min-coupling` are judged: for barely coupled modules the ratios mean little. The rule is a ranking aid, which is why it is off by default.

Fix: in the zone of pain, put an abstract interface between the module and its dependants; in the zone of uselessness, remove or merge unused abstractions.

Suppress on a line with `# pymapper: ignore[PM202]`.

### PM203 god-module

Module that is both large (`max-module-symbols`) and highly coupled (`max-module-coupling`).

- Message: `module <symbol> defines <int> classes and functions (> <int>) and is coupled to <int> modules (> <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Threshold: `thresholds.max-module-symbols` (reported above; default 80)
- Threshold: `thresholds.max-module-coupling` (reported above; default 25)
- Confidence: always 100

A module with more classes and functions (methods and nested functions included) than `max-module-symbols` whose total coupling `Ca + Ce` (distinct first-party modules importing it plus distinct first-party modules it imports, `if TYPE_CHECKING:` imports excluded) exceeds `max-module-coupling`. Such a module gathers many responsibilities and sits in the middle of the import graph, so most changes touch it and it touches most of the code base.

Fix: split the module along its responsibilities so that each dependant imports only the part it needs.

Suppress on a line with `# pymapper: ignore[PM203]`.

### PM204 god-class

Class that is both complex (`max-class-wmc`) and uses many other classes (`max-class-fan-out`).

- Message: `class <symbol> is a god class (WMC <int> > <int>, uses <int> other classes > <int>)`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Threshold: `thresholds.max-class-wmc` (reported above; default 47)
- Threshold: `thresholds.max-class-fan-out` (reported above; default 5)
- Confidence: always 100

After Lanza and Marinescu's detection strategy. WMC (weighted methods per class) is the sum of the McCabe complexities of the class's methods and class-level statements; the class fan-out is the number of distinct other classes whose constructors or methods the class's methods call (resolved calls of any confidence; calls into the class itself and to plain functions are not counted). A class above both `max-class-wmc` and `max-class-fan-out` centralises behaviour and reaches into many collaborators instead of delegating to them.

Fix: move behaviour next to the data it uses (into the collaborating classes), and split the class along groups of methods that share state.

Suppress on a line with `# pymapper: ignore[PM204]`.


## Import cycles and layering

### PM301 import-cycle

Modules that import each other, directly or indirectly.

- Message: `import cycle (<int> [module | modules]): <symbol> -> <symbol> -> ...`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: always 100

One finding per strongly connected component of the first-party module import graph, reported at an import that closes a short witness cycle. The message gives the number of modules in the component and the witness, ending at the first module again; JSON output adds a `cycle` object with the component's `size`, every `member` and the `witness`. Cycles make import order matter, cause `ImportError`s on partially initialised modules and prevent splitting code into independent parts. Imports under `if TYPE_CHECKING:` are ignored unless `analysis.type-checking-cycles` is set; function-level imports count, stub files (`.pyi`) do not.

Fix: move the shared code into a lower-level module both can import, or invert the dependency.

Suppress on a line with `# pymapper: ignore[PM301]`.

### PM302 layer-violation

Import from a lower layer into a higher one, breaking a `layers` contract.

- Message: `<symbol> imports higher layer <symbol> (contract <contract>)`
- Severity: error
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: always 100

A `kind = "layers"` contract lists layers from highest to lowest: a layer may import lower layers, never higher ones. With `containers`, each container has its own layers and imports between containers are not checked. Reported at each violating import (sibling imports under `independent-siblings` are PM304). With `transitive = true`, an import of a module outside the contract that leads to a higher layer through other such modules counts too; the message names the higher-layer module reached. Stub files (`.pyi`) are not checked.

Fix: move the imported code down a layer, or pass it in from the higher layer (dependency injection, callbacks).

Suppress on a line with `# pymapper: ignore[PM302]`.

### PM303 forbidden-import

Import breaking a `forbidden` contract.

- Message: `<symbol> imports forbidden module <name> (contract <contract>)`
- Severity: error
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: always 100

A `kind = "forbidden"` contract forbids its `source` modules (and their submodules) from importing its `forbidden` modules, which may be first- or third-party. Modules that are not analysed match unresolved imports by dotted prefix. Reported at each violating import; with `transitive = true` also at imports that lead to a forbidden module through modules outside the contract. Stub files (`.pyi`) are not checked.

Fix: remove the dependency, or move the code that needs it out of the source modules.

Suppress on a line with `# pymapper: ignore[PM303]`.

### PM304 independence-violation

Import between modules of an `independence` contract.

- Message: `<symbol> imports <symbol>, which must stay independent (contract <contract>)`
- Severity: error
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: always 100

A `kind = "independence"` contract lists modules that may not import one another (submodules included); the same applies to siblings within a layer of a `layers` contract with `independent-siblings`. Reported at each violating import; with `transitive = true` also at imports that lead to another listed module through modules outside the contract. Stub files (`.pyi`) are not checked.

Fix: move the shared code into a separate module that both may import.

Suppress on a line with `# pymapper: ignore[PM304]`.


## Dead code

### PM401 unused-function

Module-level or nested function that is never used.

- Message: `function <symbol> [is never used | is only used by unused code | is only used by code that may be unused | has no resolved callers (name appears elsewhere) | has no resolved callers (name appears in its own file) | has no resolved callers (name appears in a file that imports it) | has only possible callers (unique method name, unresolved receiver) | has no resolved callers (an unknown decorator may register it) | has no resolved callers (may override a method of an unloaded base class) | has no resolved callers (dynamic attribute access may reach it) | has no resolved callers (matches a dynamic `getattr` prefix) | is only exported (`__all__` or a re-export), never used | is never used (public name outside the public API) | is never used (public method; instances may reach callers) | has no resolved callers (name appears in a config file) | has no resolved callers (named with its module in a config file)]`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: 100 minus how live the definition looks. 100 when nothing reaches it; 50 when reached only through a `possible` call (e.g. the unique-method fallback); 40 when it carries an unknown decorator that may register it; 50 for a method of a class with a base that is not loaded (it may override a framework method); 70 when the defining module uses dynamic access (`getattr`, `globals()`, `vars()`, `__getattr__`), or for a method when the module of a base class does (name-based dispatch); in application mode 30 when listed in `__all__` or re-exported; in library mode 80 for a public name outside the public API and 50 for a public method of a class that is not part of it (its instances may reach callers). With `analysis.name-fallback`, 10 when the name occurs in its own file or in a file that imports it or its module, 50 when it occurs anywhere else (attribute, identifier, string). Callees inherit the confidence of such callers. `check` reports findings at or above `min-confidence` (default 80).

The function is not reachable from any root: nothing live calls it, references it, re-exports it or registers it (decorator entry points, presets).

Fix: delete it, or declare how it is used (`[entrypoints]`, a preset, `__all__`).

Suppress on a line with `# pymapper: ignore[PM401]`.

### PM402 unused-class

Class that is never instantiated, subclassed or referenced.

- Message: `class <symbol> [is never used | is only used by unused code | is only used by code that may be unused | has no resolved callers (name appears elsewhere) | has no resolved callers (name appears in its own file) | has no resolved callers (name appears in a file that imports it) | has only possible callers (unique method name, unresolved receiver) | has no resolved callers (an unknown decorator may register it) | has no resolved callers (may override a method of an unloaded base class) | has no resolved callers (dynamic attribute access may reach it) | has no resolved callers (matches a dynamic `getattr` prefix) | is only exported (`__all__` or a re-export), never used | is never used (public name outside the public API) | is never used (public method; instances may reach callers) | has no resolved callers (name appears in a config file) | has no resolved callers (named with its module in a config file)]`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: 100 minus how live the definition looks. 100 when nothing reaches it; 50 when reached only through a `possible` call (e.g. the unique-method fallback); 40 when it carries an unknown decorator that may register it; 50 for a method of a class with a base that is not loaded (it may override a framework method); 70 when the defining module uses dynamic access (`getattr`, `globals()`, `vars()`, `__getattr__`), or for a method when the module of a base class does (name-based dispatch); in application mode 30 when listed in `__all__` or re-exported; in library mode 80 for a public name outside the public API and 50 for a public method of a class that is not part of it (its instances may reach callers). With `analysis.name-fallback`, 10 when the name occurs in its own file or in a file that imports it or its module, 50 when it occurs anywhere else (attribute, identifier, string). Callees inherit the confidence of such callers. `check` reports findings at or above `min-confidence` (default 80).

No live code instantiates, subclasses or references the class, and no entry-point rule (base class, decorator, name) marks it as used. While PM402 is enabled, its methods are not reported separately unless they look less live than the class (higher confidence); suppressing the class finding (inline or `per-file-ignores`) hides them too.

Fix: delete it, or declare how it is used (`[entrypoints]`, a preset, `__all__`).

Suppress on a line with `# pymapper: ignore[PM402]`.

### PM403 unused-method

Method of a live class that is never called.

- Message: `method <symbol> [is never used | is only used by unused code | is only used by code that may be unused | has no resolved callers (name appears elsewhere) | has no resolved callers (name appears in its own file) | has no resolved callers (name appears in a file that imports it) | has only possible callers (unique method name, unresolved receiver) | has no resolved callers (an unknown decorator may register it) | has no resolved callers (may override a method of an unloaded base class) | has no resolved callers (dynamic attribute access may reach it) | has no resolved callers (matches a dynamic `getattr` prefix) | is only exported (`__all__` or a re-export), never used | is never used (public name outside the public API) | is never used (public method; instances may reach callers) | has no resolved callers (name appears in a config file) | has no resolved callers (named with its module in a config file)]`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: 100 minus how live the definition looks. 100 when nothing reaches it; 50 when reached only through a `possible` call (e.g. the unique-method fallback); 40 when it carries an unknown decorator that may register it; 50 for a method of a class with a base that is not loaded (it may override a framework method); 70 when the defining module uses dynamic access (`getattr`, `globals()`, `vars()`, `__getattr__`), or for a method when the module of a base class does (name-based dispatch); in application mode 30 when listed in `__all__` or re-exported; in library mode 80 for a public name outside the public API and 50 for a public method of a class that is not part of it (its instances may reach callers). With `analysis.name-fallback`, 10 when the name occurs in its own file or in a file that imports it or its module, 50 when it occurs anywhere else (attribute, identifier, string). Callees inherit the confidence of such callers. `check` reports findings at or above `min-confidence` (default 80).

The class is live but this method is never called through the class, its instances, `self` / `cls` / `super()`, or as an override of a live method in a base class. Dunder and protocol methods are always live.

Fix: delete it, or declare how it is used.

Suppress on a line with `# pymapper: ignore[PM403]`.

### PM404 unused-module

First-party module that nothing imports and that is not an entry point.

- Message: `module <symbol> [is never imported | is only imported by unused modules | is only imported by modules that may be unused | is not imported (name appears elsewhere) | is only reached through possible calls | is not imported (may be run as a script) | is not imported (name appears in a config file)]`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: 100 when no live module imports it or calls into it; 50 when reached only through `possible` calls or when its name occurs in another file (for instance `importlib.import_module("pkg.plugins.x")`); 40 for a module outside any regular package (it may be run as a script) and for modules imported only from such modules.

No first-party module imports it, it is not a script or `__main__` module, not an entry point, and (in library mode) not part of the public package. Test modules are usually entry points via the `pytest` preset. Packages (`__init__.py`) are never reported. While PM404 is enabled, the module's defs and imports are reported only when they look less live than the module (higher confidence); suppressing the module finding (`ignore[PM404]` on line 1 or `per-file-ignores`) hides them too.

Fix: delete the module, or declare it as an entry point.

Suppress on a line with `# pymapper: ignore[PM404]`.

### PM405 unused-import

Import that neither its module nor any importer of that module uses.

- Message: `<name> is imported but never used`
- Severity: warning
- Enabled by default: yes
- Cost: O(V+E)
- Confidence: 100 when no module uses the name, minus: 30 when the importing module uses dynamic access (`globals()`, `vars()`, `getattr`, `__getattr__`), 40 when another module star-imports it (public names), 40 when another file mentions both the name and the module without importing it (string references such as `mock.patch("pkg.mod.name")`), 30 for an application-mode `__init__.py` re-export, 20 in an `__init__.py` (20 more for a submodule import, often made for its side effects), 50 under `if TYPE_CHECKING:` (mostly used in quoted annotations, which name evidence sees only when the whole string is a dotted name), 10 for another conditional import.

Unlike per-file linters, this also follows re-exports: an import counts as used when another module imports the name from here or accesses it as `module.name`. Imports listed in `__all__`, the `import x as x` idiom, `__init__.py` re-exports in library mode, star imports, `__future__` imports and `try`/`except ImportError` fallbacks are kept.

Fix: delete the import, or add the name to `__all__` if it is part of the module's API.

Suppress on a line with `# pymapper: ignore[PM405]`.


## Duplication

### PM501 duplicate-code

Block of code duplicated elsewhere (token-based clone).

- Message: `<int> lines duplicated in <file>:<int>`
- Severity: info
- Enabled by default: yes, when `analysis.dupes = true`
- Cost: expensive
- Threshold: `thresholds.dupes-min-tokens` (parameter; default 50)
- Threshold: `thresholds.dupes-min-lines` (parameter; default 6)
- Confidence: Similarity of the two copies: 100 when their tokens are identical, lower when they only match after normalising identifiers and literals.

Token sequences of at least `dupes-min-tokens` tokens spanning at least `dupes-min-lines` lines that occur more than once, after normalising identifiers and literals (so renamed copies match). Reported at each copy, pointing at another copy; JSON output adds the copy's `end_line`, its `symbol` (the innermost class or function containing the whole copy, if any) and an `other` object with the other copy's `path`, `line`, `end_line` and `symbol`. Import statements are ignored, stub files (`.pyi`) are skipped, and a copy never starts or ends on an `@overload` signature or a docstring (it is trimmed to the code in between, and dropped if too little is left), so runs of overloads are not reported. Duplicates must be kept in sync by hand and drift apart.

Fix: extract the shared code into a function or base class.

Off unless `analysis.dupes = true` (token-window hashing across all first-party files).

Suppress on a line with `# pymapper: ignore[PM501]`.


## Dependencies

### PM601 undeclared-dependency

Import of a third-party distribution not declared in `pyproject.toml`.

- Message: `<name> is provided by <name>, which is not a declared dependency`
- Severity: error
- Enabled by default: yes
- Cost: O(n)
- Confidence: 100 when the import name maps to a distribution through installed dist-info metadata; lower when the mapping comes from the built-in name table or a name guess.

First-party code imports a module whose distribution is not listed in `[project.dependencies]`, `[project.optional-dependencies]` or a `[dependency-groups]` group. The code works only because the package happens to be installed.

Fix: add the distribution to the project's dependencies.

Suppress on a line with `# pymapper: ignore[PM601]`.

### PM602 unused-dependency

Declared dependency that no first-party module imports.

- Message: `dependency <name> is declared but never imported`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Confidence: 100 when the import name maps to a distribution through installed dist-info metadata; lower when the mapping comes from the built-in name table or a name guess.

A distribution in `[project.dependencies]` provides no module that first-party code imports. Plugins, CLI tools and runtime-only extras are legitimate exceptions; ignore them with `per-file-ignores` on `pyproject.toml` or an inline suppression on the dependency line.

Fix: remove the dependency, or move it to the group that needs it.

Suppress on a line with `# pymapper: ignore[PM602]`.

### PM603 private-import

Import of a private module or name of a third-party package.

- Message: `<name> is private to <name>`
- Severity: warning
- Enabled by default: yes
- Cost: O(n)
- Confidence: always 100

The import path contains a component starting with an underscore (`pkg._internal`, `from pkg import _helper`) of a third-party distribution. Private internals change without notice between releases.

Fix: use the package's public API, or pin the version and isolate the access in one module.

Suppress on a line with `# pymapper: ignore[PM603]`.

### PM604 transitive-dependency

Import of a distribution installed only as a dependency of a dependency.

- Message: `<name> is provided by <name>, installed only as a dependency of <name>`
- Severity: warning
- Enabled by default: no (enable with `select` / `extend-select`)
- Cost: O(n)
- Confidence: 100 when the import name maps to a distribution through installed dist-info metadata; lower when the mapping comes from the built-in name table or a name guess.

A refinement of PM601: the imported distribution is not declared, but another declared dependency requires it. It may disappear when that dependency changes. When this rule is enabled such imports are reported as PM604 instead of PM601; it is off by default because PM601 already reports them.

Fix: declare the distribution directly.

Suppress on a line with `# pymapper: ignore[PM604]`.

