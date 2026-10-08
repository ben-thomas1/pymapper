# Benchmarks: `cargo xtask bench`

Measures wall time per phase and peak RSS of named targets over the benchmark corpora.
Every step that touches a hot path reports these numbers (design: Performance rules).

```
cargo xtask bench                         # parse-only on tests, stdlib, venv; compare with baseline
cargo xtask bench -t read -t parse-only   # several targets
cargo xtask bench -c stdlib -n 5          # one corpus, best of 5 runs
cargo xtask bench -c mine=/path/to/code   # ad-hoc corpus
cargo xtask bench --save-baseline         # make this run the new baseline
```

`cargo xtask` is a debug build. When run from it, `bench` first builds xtask with
`--profile release` (fat LTO, so the first build takes a while) into the same target
directory and measures with that binary. `--profile profiling` measures a build with line
tables for sampling profilers. `--use-current-exe` skips the build (tests use it; debug numbers
mean nothing).

## How it measures

- Each measured run is a fresh child process (`xtask bench-run --target T --root DIR`), which
  prints one JSON record. So peak RSS (`getrusage(RUSAGE_SELF).ru_maxrss`) is per run and
  includes the process baseline (about 2.5 MB).
- `--warmup N` (default 1) unmeasured runs come first to warm the OS file cache, then `-n N`
  (default 3) measured runs. Times are **best of N**, peak RSS is **worst of N**. The results
  file also stores the median total.
- `-j N` sets `RAYON_NUM_THREADS` for the measured process. The default is all cores.
- Units: MB = 10^6 bytes everywhere (corpus size, MB/s, RSS).

## Corpora

| name     | root                                                       | override          |
|----------|------------------------------------------------------------|-------------------|
| `tests`  | `tests/corpus` in this repository                          | -                 |
| `stdlib` | `~/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13` | `PYMAPPER_STDLIB` |
| `venv`   | `~/Code/gsk/aiml-lectio/.venv/lib/python3.12/site-packages` | `PYMAPPER_VENV`   |

A corpus whose directory does not exist is skipped with a note on stderr. `-c NAME=PATH` adds
an ad-hoc corpus. Files counted are `*.py` and `*.pyi`. `__pycache__` and `.git` are not
entered, and symlinks are not followed.

## Targets

| target       | phases                 | what it does                                              |
|--------------|------------------------|-----------------------------------------------------------|
| `read`       | `walk`, `read`         | walk, then read every file in parallel (the I/O floor)    |
| `parse-only` | `walk`, `read+parse`   | same, calling the parse hook on each file's bytes         |
| `walk`       | `walk`                 | parallel `pymapper_core::walk` only, corpus options       |
| `walk-seq`   | `walk`                 | single-threaded reference walker only (same file set)     |
| `walk-git`   | `walk`                 | parallel walk with first-party options (gitignore on, hidden dirs skipped) |
| `lex`        | `walk`, `read+lex`     | same, lexing each file to EOF (parser mode, no trivia)    |
| `lex-mem`    | `walk`, `load`, `lex`  | lexer micro benchmark: load the corpus into memory, then lex it from memory (no I/O in `lex`) |
| `parse-mem`  | `walk`, `load`, `parse` | parser micro benchmark: like `lex-mem`, but runs `pymapper_core::extract::parse` with a counting visitor (counters `defs`, `calls`, `diagnostics`, `parse_cpu_ms`, `parse_MBps_core`) |
| `roots`      | `discover`, `module-map` | `pymapper_core::roots::discover` (deps + stdlib on) for the venv enclosing the corpus, then the module map over every `RECORD` file |

Walk-only targets report 0 MB (they do not stat files) and add the walker's counters
(`dirs`, `entries`, `ignored`, `hidden`; not for `walk-seq`).

The parse hook is `parse_file(src: &[u8], acc: &mut ParseAcc)` in `xtask/src/bench/targets.rs`.
Later steps plug the lexer/extractor in there. `ParseAcc` is per-thread state that gets merged
at the end; its `counters()` (e.g. tokens, defs) show up in the table and the results file.
Until then the hook does nothing, so `parse-only` costs the same as `read`. `read`, `parse-only`,
`lex`, `lex-mem` and `walk` use `pymapper_core::walk` with the corpus options: no gitignore, hidden directories
entered, `__pycache__`/`.git` skipped, symlinks not followed. That is the same file set as the
sequential reference walker in `xtask/src/bench/walk.rs` (`walk-seq`), so the two are directly
comparable. `walk-git` uses `WalkOptions::default()` (what first-party roots get); point it at a
repository with `-c NAME=PATH`, since on a venv the enclosing `.gitignore` usually ignores
everything.

The lexer targets add counters: `tokens`, `error_tokens`, `lex_cpu_ms` (time inside the lexer,
summed over threads) and `lex_MBps_core` (corpus MB per second of lexer time). For a clean
single-core number run `lex-mem` with `-j 1`: the `lex` phase is then pure lexing on one
thread (`MB / lex`). `lex-mem` holds the whole corpus in memory, so its peak RSS is about the
corpus size and says nothing about the lexer's own memory use (a few KB per lexer).

Adding a target: add a `Target` variant and a match arm in `targets::run`.

## Phase timing API (`pymapper_core::timing`)

Phases come from a tiny always-compiled timer in core. Core code under test reports phases the
same way, and they show up as extra table columns automatically.

```rust
use pymapper_core::timing;

let _g = timing::phase("resolve");            // recorded when the guard drops
let facts = timing::time("extract", || run()); // or wrap a closure
```

- Disabled by default. While disabled, `phase` is one relaxed atomic load and nothing else.
  `bench-run` turns it on with `timing::enable(true)` and reads it with `timing::take()`.
- Record phases on the coordinating thread, around a whole parallel section, so that durations
  are wall time. Calling it per file inside a rayon closure would sum CPU time and take the
  lock once per file.
- Phases with the same name add up and keep the order in which they were first recorded.

## Output

```
corpus  target      files     MB    walk  read+parse   total  MB/s  peak RSS MB
stdlib  parse-only   1043   15.8  15.9ms      28.8ms  45.0ms   351         5.21
```

One column per phase seen in any row (`-` when a row lacks it), the total wall time of the
target, MB/s (corpus MB / total), and peak RSS.

## Results file and regressions

- Each run writes `bench-results/latest.json` (override with `--out PATH`). `bench-results/` is
  ignored by git.
- If `bench-results/baseline.json` exists (override with `--baseline PATH`), the run is compared
  with it, matching rows by (corpus, target). Use `--no-compare` to skip this.
- A metric (each phase, total, peak RSS) counts as a **regression** when it is worse by more
  than `--threshold` percent (default 10) **and** by at least `--min-delta-ms` (default 5) or
  `--min-delta-mb` (default 2). The absolute floor keeps noise on tiny corpora from being
  flagged.
- Notes are printed when the corpus changed since the baseline (file count or size), when the
  thread count changed, or when a row or phase is new.
- `--save-baseline` writes this run to the baseline file after comparing with the old one.
- Exit codes: 0 = no regressions, 1 = regressions flagged, 2 = usage or fatal error (including
  a corrupt baseline).

Results file layout (`schema` 1):

```json
{
  "schema": 1, "created_unix": 1791400000, "git_rev": "7e3c9e98035f-dirty",
  "os": "macos", "arch": "aarch64",
  "results": [{
    "corpus": "stdlib", "target": "parse-only", "root": "/...",
    "files": 1043, "bytes": 15812345, "errors": 0, "threads": 14, "runs": 3,
    "phases": [{"name": "walk", "s": 0.0159}, {"name": "read+parse", "s": 0.0288}],
    "total_s": 0.045, "total_median_s": 0.046, "peak_rss_bytes": 5210112,
    "counters": []
  }]
}
```

## Reference numbers

Measured with S0.2 (walk + read only) on a 14-core Apple Silicon Mac, release build, warm cache,
best of 3. Other worktrees were building at the same time, so the machine was not idle.

| corpus | target     | files  | MB    | walk   | read / read+parse | total  | MB/s | peak RSS MB |
|--------|------------|--------|-------|--------|-------------------|--------|------|-------------|
| tests  | read       | 29     | 0.01  | 0.29ms | 0.93ms            | 1.37ms | 6    | 2.69        |
| tests  | parse-only | 29     | 0.01  | 0.41ms | 1.10ms            | 1.52ms | 5    | 2.65        |
| stdlib | read       | 1043   | 15.8  | 15.7ms | 29.6ms            | 45.3ms | 348  | 5.67        |
| stdlib | parse-only | 1043   | 15.8  | 15.9ms | 28.8ms            | 45.0ms | 351  | 5.21        |
| venv   | read       | 60529  | 650.1 | 2.93s  | 1.89s             | 4.86s  | 134  | 24.0        |
| venv   | parse-only | 60529  | 650.1 | 2.75s  | 1.92s             | 4.71s  | 138  | 23.3        |

### Walker (S2.2)

`pymapper_core::walk` vs the single-threaded reference walker, same file set, 14-core Apple
Silicon Mac, release build, warm cache, best of 5 (load average about 9 from other builds, so
absolute times are noisy). `lectio` is the repository that contains the venv, walked with
first-party options (`walk-git`: gitignore on, hidden directories skipped) and with corpus
options (`walk`: everything, including `.venv` and `node_modules`).

| corpus | target   | files | dirs  | walk    | peak RSS MB |
|--------|----------|-------|-------|---------|-------------|
| stdlib | walk     | 1043  | 105   | 2.04ms  | 3.44        |
| stdlib | walk-seq | 1043  | 105   | 3.19ms  | 3.06        |
| venv   | walk     | 60529 | 19618 | 783.5ms | 20.4        |
| venv   | walk-seq | 60529 | 19618 | 2.25s   | 13.2        |
| lectio | walk-git | 1803  | 914   | 43.2ms  | 3.82        |
| lectio | walk     | 93289 | 30543 | 1.37s   | 38.0        |

Thread scaling on the venv (`-j N`, best of 3): 1 thread 2.79s, 2 threads 1.51s, 4 threads
842ms, 8 threads 807ms, 14 threads 784ms. It flattens after about 4 threads, most likely because
the filesystem calls (`opendir` on full paths, `getdirentries`) serialise in the kernel (not
profiled; the machine was also loaded). The `walk-git` file set equals
`git ls-files --cached --others --exclude-standard` filtered to non-hidden `*.py`/`*.pyi`
(1803 files).

Peak RSS above `walk-seq` comes from the exactly sized sorted output being built while the
per-thread lists (relative paths) are still alive; the output itself is one byte buffer plus
one `u32` per file.

### Lexer (S1.1)

Same machine, release build, warm cache, best of 5. Other worktrees were
building at the same time (load average about 10 on 14 cores), so treat these as lower bounds.
`lex` MB/s/core is the `lex_MBps_core` counter (MB per second of time inside the lexer,
summed over all threads). The `-j 1` rows are one thread lexing from memory: MB / `lex` phase.

| corpus | target       | threads | files | MB    | lex phase | lex MB/s | tokens     | peak RSS MB |
|--------|--------------|---------|-------|-------|-----------|----------|------------|-------------|
| stdlib | lex          | 14      | 1043  | 15.8  | -         | 653/core | 2,044,807  | 5.72        |
| stdlib | lex-mem      | 14      | 1043  | 15.8  | 1.82ms    | 8,700    | 2,044,807  | 22.9        |
| stdlib | lex-mem `-j 1` | 1     | 1043  | 15.8  | 20.5ms    | 771      | 2,044,807  | 21.5        |
| venv   | lex          | 14      | 60529 | 650.1 | -         | 745/core | 83,594,594 | 24.4        |
| venv   | lex-mem      | 14      | 60529 | 650.1 | 62.1ms    | 10,470   | 83,594,594 | 793.7       |
| venv   | lex-mem `-j 1` | 1     | 60529 | 650.1 | 668ms     | 973      | 83,594,594 | 666.6       |

Read + lex costs about the same as read alone (stdlib 46.2ms vs 42.3ms, venv 4.92s vs 4.83s),
and peak RSS does not change: the lexer allocates nothing per token.

### Import roots (S2.4)

`cargo xtask bench -t roots -c venv -n 5`: `discover` finds the venv enclosing the corpus
(nearest ancestor with `pyvenv.cfg`), the stdlib, parses all 504 `*.dist-info` directories
(`METADATA` prefix, `RECORD` = 9.7 MB, `top_level.txt`, `direct_url.json`) and the `.pth` files;
`module-map` runs `ModuleMapBuilder` over the `RECORD` `.py`/`.pyi` files each distribution owns
(60527 files, 62522 dotted names; one file is listed by two distributions and is added once). 14-core Apple Silicon Mac, release build, warm cache, best of 5, load average
about 2-3 from other work.

| threads | discover | module-map | total   | peak RSS MB |
|---------|----------|------------|---------|-------------|
| 14      | 42.9ms   | 12.5ms     | 55.6ms  | 28.9        |
| 4       | 47.9ms   | 12.3ms     | 60.5ms  | 28.3        |
| 1       | 126.2ms  | 12.3ms     | 138.5ms | 26.7        |

Heap after the run: `Distributions` 7.1 MB (per-site interned `RECORD` paths, owners, top-level
names), `ModuleMap` 8.5 MB. `discover` is dominated by opening and reading the metadata files
(a plain sequential `read` of every `METADATA` alone took 40-110 ms in isolation, while parsing
all of `RECORD` costs about 14 ms of CPU); like the walker it stops scaling at about 4 threads.
`module-map` is single-threaded (interning plus one sort of the (name, root) entries). The
14-thread row was re-measured after `add_record_files` switched to owned files (S2.4 review
fixes); the other rows are from the first measurement.

### Parser (S1.2)

Same machine, release build, warm cache, best of 5, load average about 5-7 from other
worktrees. `parse-mem` runs `pymapper_core::extract::parse` (lexer included) with a visitor that
counts defs, calls and diagnostics; positions (line/col) are computed for every event. `parse
MB/s/core` is the `parse_MBps_core` counter. The `-j 1` rows are one thread parsing from memory.
The lexer rows were measured in the same session for comparison.

| corpus | target           | threads | files | MB    | lex / parse phase | MB/s/core | defs    | calls     | diagnostics |
|--------|------------------|---------|-------|-------|-------------------|-----------|---------|-----------|-------------|
| stdlib | lex-mem          | 14      | 1043  | 15.8  | 2.21ms            | 596       | -       | -         | -           |
| stdlib | parse-mem        | 14      | 1043  | 15.8  | 4.47ms            | 274       | 24,394  | 83,013    | 0           |
| stdlib | lex-mem `-j 1`   | 1       | 1043  | 15.8  | 21.2ms            | 736       | -       | -         | -           |
| stdlib | parse-mem `-j 1` | 1       | 1043  | 15.8  | 43.0ms            | 364       | 24,394  | 83,013    | 0           |
| venv   | lex-mem          | 14      | 60529 | 650.1 | 67.2ms            | 700       | -       | -         | -           |
| venv   | parse-mem        | 14      | 60529 | 650.1 | 146ms             | 317       | 927,612 | 3,496,133 | 14          |
| venv   | lex-mem `-j 1`   | 1       | 60529 | 650.1 | 739ms             | 881       | -       | -         | -           |
| venv   | parse-mem `-j 1` | 1       | 60529 | 650.1 | 1.55s             | 419       | 927,612 | 3,496,133 | 14          |

Parsing costs about 2x lexing: 364-419 MB/s on one core, against 217 MB/s/core for ruff's
parser in the baseline. Of the parser's own time, the line cursor is about 14% and the
expression scanner about 24% (samply, venv, `-j 1`). All 14 venv diagnostics are in 4 files
of genuine Python 2 code (`print >>f, x`). Peak RSS in `parse-mem` is the loaded corpus. The
parser itself uses a handful of reused scratch vectors per file and allocates nothing per token.

### Facts (S1.3)

`cargo bench -p pymapper-core --bench facts -- [--spans | --parse-only] [--reps N] DIR` (own
harness in `crates/pymapper-core/benches/facts.rs`, not yet an xtask target; S1.5 wires the
extractor into `xtask bench`). Each rayon worker owns a `FactStore` and an `Interner` and runs
read -> `extract` -> drop source per file, keeping every file's facts (the loader's shape).
`--parse-only` runs the parser with a no-op visitor instead (the extractor's baseline);
`--spans` uses the pre-interning span sink. MB/s/core is MB per second of time inside
`extract` (or `parse`), summed over threads. Peak RSS is of one run that retains all facts and
interners (earlier runs are dropped first). Same machine, release build, warm cache, best of
3-5, load average 4-10 from other worktrees, so treat times as lower bounds.

| corpus | threads | sink       | wall    | MB/s/core | parse-only MB/s/core | peak RSS MB |
|--------|---------|------------|---------|-----------|----------------------|-------------|
| stdlib | 14      | interner   | 31.3ms  | 203       | 295                  | 22.0        |
| stdlib | 1       | interner   | 149ms   | 246       | 330                  | -           |
| stdlib | 1       | spans      | 136ms   | 287       | 330                  | -           |
| venv   | 14      | interner   | 1.95s   | 220       | 303                  | 494         |
| venv   | 1       | interner   | 8.45s   | 238       | 347                  | -           |

Wall includes reading the files (the venv wall is I/O bound). Fact building costs about 17% on
top of the parser with span strings, about 35% with interning (the interner's hash and probe
per recorded name; S2.3 needs that anyway). Profiled hot spots were a separate line-count pass
and a hash set for evidence; both were replaced (the parser's line cursor gives the line
count, a direct-mapped filter plus sort/dedup gives the evidence column).

Memory per LOC (rows only, `Sym` = 4 B; layout in `extract/facts.rs`):

| table        | stdlib rows/kLOC | stdlib B/LOC | venv rows/kLOC | venv B/LOC |
|--------------|------------------|--------------|----------------|------------|
| defs         | 58.3             | 2.97         | 58.0           | 2.96       |
| calls        | 190.1            | 3.61         | 205.1          | 3.90       |
| bindings     | 153.0            | 3.21         | 136.1          | 2.86       |
| names        | 311.6            | 1.25         | 298.7          | 1.19       |
| evidence     | 278.6            | 1.11         | 244.1          | 0.98       |
| imports      | 23.0             | 0.62         | 52.9           | 1.43       |
| params       | 99.8             | 0.50         | 101.9          | 0.51       |
| other        |                  | 0.14         |                | 0.21       |
| **total**    |                  | **13.41**    |                | **14.01**  |

Plus per-file headers (112 B per file): 0.27 B/LOC stdlib, 0.40 B/LOC venv. LOC: stdlib
436,648, venv 17,044,697. The per-thread tables grow by doubling, so their heap is about
19-21 B/LOC until `FactStore::shrink_to_fit` (or the merge into global tables) trims them to
the row size; that slack and the interners (47.5 MB on the venv) are most of the venv's peak
RSS above the 239 MB of rows. Two layout choices brought the stdlib from 16.5 to 13.4 B/LOC:
chain lengths as `u8` columns instead of 8-byte runs, and single-name chains stored inline in
the chain column (most binding targets and many callees) instead of in `names`.
