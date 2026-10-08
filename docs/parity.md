# Parity with ruff: `cargo xtask diff-ruff`

`diff-ruff` runs ruff's parser (`ruff_python_parser`, the oracle) and our front end
(`pymapper_core::extract`: lexer, parser, facts and metrics in one pass) over the same files,
normalises both into one comparable fact model and diffs them per file.

```
cargo xtask diff-ruff DIR                 # ours vs ruff (exit 1 on any mismatch)
cargo xtask diff-ruff DIR --examples 20   # more examples per mismatch category
cargo xtask diff-ruff FILE --dump --ours  # our normalised facts (without --ours: ruff's)
cargo xtask diff-ruff DIR --self-check    # ruff vs ruff (tests the pipeline, expects 0)
cargo xtask diff-ruff DIR --oracle-only   # ruff totals only
```

## What is compared

The model and its conventions are documented in `xtask/src/diff_ruff/facts.rs`:

| fact    | fields compared                                                                 |
|---------|---------------------------------------------------------------------------------|
| defs    | kind (function / async function / class), qualname, line of the name, decorators text, bases text, parameter count, McCabe (C901) of functions |
| imports | scope, module, name, alias, level, line (one fact per imported name)           |
| calls   | scope, callee chain (`a.b.c`, `self.m`, `super().m`, `?.m`, `?`), line          |

Defs are matched by qualname in source order; imports and calls are compared as multisets, so
a missing, extra or changed fact each show up with file and line.

Our side (`xtask/src/diff_ruff/ours.rs`) reads the core fact tables. Two fields are not kept
as text by the core (it keeps what the analyses need: chains and flags), so the adapter
rebuilds them and thereby still checks the core's rows:

- decorators: the decorator's chain from the facts (`a.b`), plus for a `CALLED` decorator the
  normalised source from its `(`; a `COMPLEX` decorator uses its normalised source. A wrong
  chain, flag or decorator count changes the text.
- bases: the class's argument list is re-lexed and split at top-level commas; plain-chain
  items are rebuilt from their rows (`kw=` + chain), other items (subscripted, starred,
  complex) use the normalised source, and a row count that differs from the item count is
  marked (`!rows=N`).

Not compared (no ruff equivalent, covered by unit and golden tests in `pymapper-core`):
bindings, `__all__`, global/nonlocal, evidence, suppressions, diagnostics, def/import/call
flags, cognitive complexity, Halstead, nesting, LOC.

## Results

Release build, 14-core Mac. Every mismatch found on the way was triaged; all were fixed in the
core (see below) or are documented intentional differences.

| corpus      | files  | compared | defs    | imports | calls     | mismatches |
|-------------|--------|----------|---------|---------|-----------|------------|
| tests/corpus| 29     | 29       | 73      | 29      | 84        | 0          |
| stdlib 3.13 | 1043   | 1043     | 24,394  | 10,040  | 83,013    | 0          |
| venv        | 60,529 | 60,527   | 927,531 | 901,139 | 3,496,134 | 0          |

The two venv files ruff cannot parse are not compared (we extract them anyway; the front end
never fails a file):

- `joblib/test/test_func_inspect_special_encoding.py`: not UTF-8 (a latin-1 test file).
- `rdkit-stubs/Chem/rdRGroupDecomposition.pyi`: invalid annotated assignment target.

## Bugs found and fixed

- **Parenthesised primaries** (stdlib, 2 calls): `(num).bit_length()` gave the unknown receiver
  `?.bit_length`. Parentheses around a whole primary are now transparent to the callee chain,
  as in the AST: `(a).b()` -> `a.b`, `((a.b)).c()` -> `a.b.c`, `(self).m()` -> `self.m`,
  `(f().g).h()` -> `?.g.h`. Anything else in the group (operators, commas, lambdas, f-strings,
  `await`, ...) still makes the receiver unknown.
- **Python 2 statements** (venv, 6 calls): after reporting `print >>f, x` / `print x` /
  `exec code` as Python 2, the parser skipped the rest of the statement, losing its calls.
  `print >>f, x` is even valid Python 3 (a tuple expression), which ruff parses. The rest of
  the statement is now scanned as an expression (the diagnostic stays).
- **End-of-input diagnostics** (found by `xtask fuzz -t extract`): a diagnostic at end of input
  sat on the line after the last one when the file ended with a line end. Facts now put it at
  the end of the last line.

## Intentional differences

- **Call line**: the line of the first name of the callee chain (`a` in `a.b.c()`, `m` in
  `super().m()` / `x[0].m()`), and of the `(` only when the chain is empty (`f()()`). ruff's
  natural choice is the `(`; the two differ for chains split over lines (`wheel_name.\n
  removeprefix(...)`). The core stores the callee position because that is what a finding
  points at, and storing the `(` as well would cost 4 bytes per call row (about 0.8 B/LOC).
  The oracle reports the same position, so the diff checks it exactly.
- **Python 2 code** is unsupported (design): it gets a `Python2` diagnostic, and calls in
  the rest of the statement are still recorded. ruff rejects such files (syntax error) unless
  the statement happens to be valid Python 3 (`print >>f, x`), where both sides agree.
- **Class McCabe**: our metrics row for a class holds the sum over its body (a WMC-like
  number); C901 scores functions only, so the model compares functions only (`mccabe = 0` for
  classes on both sides).
- **Unparseable files**: ruff skips non-UTF-8 files and files with syntax errors; we extract
  them with diagnostics. They are listed as "skipped by ruff", not compared.
