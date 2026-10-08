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
| `parse-only` | `walk`, `read+parse`   | the front end in the loader's shape: per thread, read each file into a reused buffer, lex + parse + facts + metrics in one pass (interned names), drop the source; all facts are kept to the end (`xtask/src/bench/extract.rs`) |
| `walk`       | `walk`                 | parallel `pymapper_core::walk` only, corpus options       |
| `walk-seq`   | `walk`                 | single-threaded reference walker only (same file set)     |
| `walk-git`   | `walk`                 | parallel walk with first-party options (gitignore on, hidden dirs skipped) |
| `lex`        | `walk`, `read+lex`     | same, lexing each file to EOF (parser mode, no trivia)    |
| `lex-mem`    | `walk`, `load`, `lex`  | lexer micro benchmark: load the corpus into memory, then lex it from memory (no I/O in `lex`) |
| `parse-mem`  | `walk`, `load`, `parse` | parser micro benchmark: like `lex-mem`, but runs `pymapper_core::extract::parse` with a counting visitor (counters `defs`, `calls`, `diagnostics`, `parse_cpu_ms`, `parse_MBps_core`) |
| `metrics-mem` | `walk`, `load`, `parse` | like `parse-mem`, with `pymapper_core::extract::metrics` teed into the same pass; every metrics row is kept to the end (extra counters `metric_rows`, `metrics_kb` = heap of the kept tables) |
| `extract-mem` | `walk`, `load`, `parse` | the extractor micro benchmark: `parse-only`'s front end from memory (no I/O in `parse`). `XTASK_EXTRACT_STAGES=parse\|metrics\|facts-spans\|facts\|all` runs only part of it, to cost each stage |
| `roots`      | `discover`, `module-map` | `pymapper_core::roots::discover` (deps + stdlib on) for the venv enclosing the corpus, then the module map over every `RECORD` file |
| `load`       | `walk`, `load.extract`, `load.merge`, `load.build` | end-to-end `pymapper_core::index::load` with the corpus root as the import root of one group: parallel read -> extract -> intern -> drop source, in-order merge into the global tables, interner merge + remap, hierarchy. The whole index is alive at the end (counters `loc`, `defs`, `nodes`, `strings`, `index_kb` = `Index::heap_bytes`, `facts_kb`, `metrics_kb`, `intern_kb`, `index_B_per_kLOC`) |
| `resolve`    | `walk`, `load.*`, `resolve.modules`, `resolve.imports`, `resolve.edges` | `load`, then `pymapper_core::resolve::resolve_imports` (module table + namespaces, per-row resolution, module edges) with the built-in stdlib table as the only attribution. Counters `imports`, `modules`, shares of import rows in per mille (`found_pm`, `dynamic_pm`, `missing_pm`, `external_pm`), `module_edges`, `ext_edges`, `resolve_kb` = `Imports::heap_bytes`, `index_kb` |
| `calls`      | `walk`, `load.*`, `resolve.modules`, `resolve.imports`, `resolve.edges`, `resolve.classes`, `resolve.mro`, `resolve.calls` | `resolve`, then `pymapper_core::resolve::resolve_calls` (class table + method index, bases + C3 MROs, then every call row, uncalled decorator and override in parallel per file). Counters `calls`, shares of call rows in per mille (`certain_pm`, `likely_pm`, `possible_pm`, `unresolved_pm`; of which `builtin_pm` = builtins and builtin-type methods, `external_pm` = unloaded code and unloaded bases), `edges`, `override_edges`, `decorator_edges`, `classes`, `incomplete_classes`, `calls_kb` = `ResolvedCalls::heap_bytes`, `resolve_kb`, `index_kb` |
| `graph`      | `calls` phases, `graph.build`, `graph.scc`, `graph.modules` | `calls`, then `pymapper_core::graph`: `graph.build` = `call_graph` over every node (all edges with a target), `graph.scc` = its `Condensed` (iterative Tarjan + DAG), `graph.modules` = `import_graph` (no flag mask) + `module_call_graph` and both condensations. Counters `graph_nodes`, `graph_edges` (distinct), `sccs`, `cyclic_sccs`, `largest_scc`, `dag_edges`, `modules`, `import_edges`, `import_cycles` and `module_call_cycles` (components with 2+ modules), `module_call_edges`, `callgraph_kb` (call graph + condensation heap), `graph_kb` (all six structures), `calls_kb`, `index_kb` |
| `load-cold`  | inputs, `load.extract`, `load.merge`, `load.build`, `cache.write` | `pymapper_core::cache::load` after clearing the cache: the `load` pipeline plus encoding and atomically writing every segment. Inputs are built the way the CLI builds them: a venv's site-packages through import roots (phases `discover`, `files`: file lists from `RECORD`, no walk of distribution files; one trusted segment per distribution), a stdlib through a walk without the base interpreter's `site-packages` plus import roots (one trusted segment per interpreter), anything else as one first-party group validated per file (`walk`). The cache lives in `$TMPDIR/pymapper-bench-cache/<corpus hash>`. Counters: `segments`, `opened`, `hits`, `verified`, `misses`, `written`, `opened_kb`, `written_kb`, `index_kb` |
| `load-warm`  | same as `load-cold`    | like `load-cold` but keeps the cache, so after the warm-up run every file is a cache hit (no read, no parse; first-party files cost one `stat` each) |

Walk-only targets report 0 MB (they do not stat files) and add the walker's counters
(`dirs`, `entries`, `ignored`, `hidden`; not for `walk-seq`).

`parse-only` and `extract-mem` report the counters `loc`, `defs`, `calls`, `diagnostics`,
`parse_cpu_ms` (time inside the extractor, summed over threads), `parse_MBps_core` (corpus MB
per second of that time: one core's throughput), `facts_B_per_kLOC` and `metrics_B_per_kLOC`
(retained row bytes per 1000 LOC, i.e. milli-bytes per LOC; facts include the 112-byte file
headers), `facts_kb`, `metrics_kb` and `intern_kb` (heap of the per-thread interners). `read`, `parse-only`,
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
| other        |                  | 0.14         |                | 0.18       |
| **total**    |                  | **13.41**    |                | **14.01**  |

Plus per-file headers (112 B per file): 0.27 B/LOC stdlib, 0.40 B/LOC venv. LOC: stdlib
436,648, venv 17,044,697. The per-thread tables grow by doubling, so their heap is about
19-21 B/LOC until `FactStore::shrink_to_fit` (or the merge into global tables) trims them to
the row size; that slack and the interners (47.5 MB on the venv) are most of the venv's peak
RSS above the 239 MB of rows. Two layout choices brought the stdlib from 16.5 to 13.4 B/LOC:
chain lengths as `u8` columns instead of 8-byte runs, and single-name chains stored inline in
the chain column (most binding targets and many callees) instead of in `names`.

### Metrics (S1.4)

Same machine, release build, warm cache, load average about 5-7 from other worktrees.
`metrics-mem` is `parse-mem` plus the metrics visitor (McCabe, cognitive, nesting, LOC,
Halstead, MI, params per definition) via `Tee`, so the difference between the two rows is the
metrics cost. 14-thread rows best of 5, `-j 1` rows best of 3 (stdlib `-j 1`: best of 10).

| corpus | target             | threads | parse phase | MB/s/core | peak RSS MB | metrics rows | metrics_kb |
|--------|--------------------|---------|-------------|-----------|-------------|--------------|------------|
| stdlib | parse-mem          | 14      | 4.42ms      | 282       | 23.9        | -            | -          |
| stdlib | metrics-mem        | 14      | 6.01ms      | 201       | 29.2        | 24,394       | 1,418      |
| stdlib | parse-mem `-j 1`   | 1       | 43.0ms      | 364       | 22.3        | -            | -          |
| stdlib | metrics-mem `-j 1` | 1       | 57.1ms      | 268       | 24.8        | 24,394       | 1,474      |
| venv   | parse-mem          | 14      | 154ms       | 305       | 825.4       | -            | -          |
| venv   | metrics-mem        | 14      | 217ms       | 225       | 884.9       | 927,612      | 54,920     |
| venv   | parse-mem `-j 1`   | 1       | 1.63s       | 394       | 674.9       | -            | -          |
| venv   | metrics-mem `-j 1` | 1       | 2.18s       | 293       | 848.4       | 927,612      | 55,050     |

Metrics add about 33-40% to parse time; parse + metrics still runs at 268-293 MB/s on one
core, above ruff's parser alone (217 MB/s/core). About half of the visitor's
time is the operand path (text hash plus one probe of the per-file last-seen table); operators
use a `u128` kind bitset per open definition. A row is 40 bytes (9 x u32/f32 + 2 x u16), so the
venv's 928k definitions need 37 MB raw; the bench's per-job tables grow by doubling, and the
`-j 1` peak RSS (+174 MB) also includes the freed generations of one large growing table.
Storing metrics inside per-file def records (S1.3) avoids that growth pattern.

### Front end (S1.5)

The whole front end (lex + parse + facts + metrics in one pass, interned names) as the loader
will run it. 14-core M4 Pro (10 performance + 4 efficiency cores), release build, warm cache;
load average about 3-4 from other worktrees, so times are lower bounds.

Cost of each stage on one core (stdlib, `extract-mem -j 1` with `XTASK_EXTRACT_STAGES`, best of
5; the lexer row is `lex-mem -j 1`):

| stages (`XTASK_EXTRACT_STAGES`)              | parse phase | MB/s/core |
|----------------------------------------------|-------------|-----------|
| lexer only (`lex-mem`)                       | 19.9ms      | 794       |
| lexer + parser, no-op visitor (`parse`)      | 35.8ms      | 441       |
| lexer + parser + metrics (`metrics`)         | 49.0ms      | 322       |
| lexer + parser + facts, span strings (`facts-spans`) | 46.7ms | 338   |
| lexer + parser + facts, interned (`facts`)   | 52.1ms      | 303       |
| everything (`all`, what `parse-only` runs)   | 63.4ms      | 249       |

Throughput and memory (best of 3; `-j 1` stdlib `extract-mem` best of 10). MB/s/core is
`parse_MBps_core` (MB per second of time inside the extractor, summed over threads); on 14
threads it averages over performance and efficiency cores, which share caches and memory
bandwidth.

| corpus | target        | threads | walk    | load / read+parse | parse phase | total   | MB/s/core | peak RSS MB |
|--------|---------------|---------|---------|-------------------|-------------|---------|-----------|-------------|
| stdlib | `extract-mem` | 1       | 14.4ms  | 82.3ms            | 67.5ms      | 169.4ms | 234       | 34.8        |
| stdlib | `extract-mem` | 14      | 5.30ms  | 29.1ms            | 8.18ms      | 43.4ms  | 150       | 39.1        |
| stdlib | `parse-only`  | 1       | 14.2ms  | 160.4ms           | -           | 176.5ms | 205       | 17.5        |
| stdlib | `parse-only`  | 14      | 5.70ms  | 39.2ms            | -           | 45.2ms  | 170       | 23.9        |
| venv   | `extract-mem` | 1       | 2.97s   | 5.43s             | 2.67s       | 11.08s  | 243       | 981.9       |
| venv   | `extract-mem` | 14      | 892ms   | 1.92s             | 251ms       | 3.08s   | 185       | 1229.9      |
| venv   | `parse-only`  | 1       | 2.97s   | 8.40s             | -           | 11.38s  | 223       | 346.8       |
| venv   | `parse-only`  | 14      | 937ms   | 2.12s             | -           | 3.06s   | 179       | 442.5       |

- **One core**: 234-249 MB/s on the stdlib (63.4ms in a quieter run, table above), 243 MB/s
  on the venv. The design target of 400 MB/s/core is **not met**.
- **All cores, from memory**: 1.93 GB/s on the stdlib (15.8 MB in 8.18ms), 2.59 GB/s on the
  venv (650 MB in 251ms).
- **All cores, from disk** (`parse-only`): the venv's `read+parse` phase (2.12s) costs the same
  as reading alone (`read`: 2.17s in the same session), so the front end is hidden behind I/O;
  walk + read + extract of 16M LOC takes about 3.1s.

Why 400 MB/s/core is missed (samply on the `profiling` build, venv, `-j 1`, 2.5k samples in
the extractor): the profile is flat. The lexer is about a third (`next_token`, `lex_token`,
`name_or_string`, identifier scan), the parser and expression scanner about a quarter (ring
buffer `peek`/`fill`, postfix-chain state), the metrics visitor about 13%, interning about 7%
(`fx_words` hash and probe) and fact building about 10% (evidence hashing, call/binding rows);
no function is above 7% self time. The lexer alone already runs at 794 MB/s/core and lexer +
parser with a no-op visitor at 441 MB/s/core, so 400 MB/s/core for the full extractor would
need roughly halving the lexer and parser costs, not a hot-spot fix. Tried and measured in S1.5:
precomputing line starts per file instead of the forward line cursor (no change, reverted:
it allocated per file); removing the Halstead distinct-operand tracking entirely (about 6% of
the metrics-only pass, within noise for the whole extractor); inlining the parser's ring fill
(kept from the interrupted attempt, not re-measured). Ideas left for later: fusing the
identifier hash into the lexer's name scan
(the same identifier is hashed up to three times: evidence, Halstead operand, interner), and a
SIMD identifier/whitespace scan in the lexer.

Memory: bytes retained per LOC (rows without spare capacity; facts include the 112-byte file headers):

| corpus | LOC        | facts B/LOC | metrics B/LOC | total B/LOC | facts + metrics MB | interner MB |
|--------|------------|-------------|---------------|-------------|--------------------|-------------|
| stdlib | 436,648    | 12.92       | 1.28          | 14.20       | 6.2                | 1.9         |
| venv   | 17,044,697 | 13.70       | 1.25          | 14.95       | 254.8              | 48.6        |

venv facts by table: calls 3.69, bindings 2.72, defs 2.49, imports 1.43, names 1.19, evidence
0.98, params 0.51, headers 0.40, decorators 0.14, bases 0.12, the rest 0.03. At the start of
S1.5 the venv needed 15.67 B/LOC (14.41 facts + 1.25 metrics); three layout changes brought it
under the 15 B/LOC target: the call receiver packed into the call flags byte (-0.21), the
binding value kind packed into the binding flags byte (-0.14), and the per-def decorator/base
start columns (8 B on every def) replaced by an owner column on the rare decorator and base
rows (-0.34). Interner bytes are per thread here (each worker interns its own files); the
global merge (S2.3) deduplicates them. Peak RSS of `parse-only` above the retained rows is the
per-thread tables' doubling slack and the interners; `extract-mem` also holds the whole
corpus in memory.

### Load / index (S2.3)

End-to-end `pymapper_core::index::load` (`load` target): walk, then parallel read -> extract
-> intern -> drop source, merge into the global tables, hierarchy. Module names come from the
corpus root (`Inputs::from_root_dir`). 14-core M4 Pro, release, warm cache, best of 3 (RSS
worst of 3). Load average was about 5-7 from other worktrees, so times are upper bounds. The
`read` and `parse-only` rows come from the same session.

| corpus | target       | threads | walk    | load.extract | load.merge | load.build | total   | peak RSS MB |
|--------|--------------|---------|---------|--------------|------------|------------|---------|-------------|
| stdlib | `load`       | 1       | 16.1ms  | 158.5ms      | 0.04ms     | 0.58ms     | 175.4ms | 19.4        |
| stdlib | `load`       | 14      | 5.51ms  | 34.3ms       | 1.10ms     | 0.58ms     | 41.8ms  | 27.4        |
| stdlib | `parse-only` | 14      | 4.97ms  | 32.6ms (read+parse) | -   | -          | 37.9ms  | 23.3        |
| stdlib | `read`       | 14      | 5.06ms  | 29.1ms (read) | -         | -          | 34.4ms  | 5.9         |
| venv   | `load`       | 1       | 2.88s   | 8.37s        | 0.10ms     | 45.1ms     | 11.38s  | 385.7       |
| venv   | `load`       | 14      | 869ms   | 2.12s        | 25.4ms     | 50.3ms     | 3.09s   | 497.5       |
| venv   | `parse-only` | 14      | 929ms   | 2.09s (read+parse) | -    | -          | 3.06s   | 440.0       |
| venv   | `read`       | 14      | 942ms   | 1.92s (read) | -          | -          | 2.86s   | 26.9        |

- **Time**: the load costs the same as `parse-only` (the front end with no merge) and is
  I/O-bound on the venv: `load.extract` (2.12s) is within 10% of reading alone (1.92s). The
  merge (interner merge, Sym remap, shrink) is 25ms and the hierarchy 50ms for 17M LOC
  (998k nodes), about 2.5% of the total. Only `Interner::merge_all` is parallel; the remap
  and the hierarchy run on one thread.
- **Retained memory** (`Index::heap_bytes`, venv): 346 MB = 20.3 B/LOC. Facts 226.7 MB
  (13.3 B/LOC, exact size), interner 50.5 MB (870k strings, deduplicated over threads),
  metrics 21.3 MB, nodes about 25 MB (25 B/node), plus the paths, file headers and module table.
- **Peak RSS** (venv, 14 threads): 497.5 MB, 151 MB above the retained index. That is mostly
  the worker interners (about 80 MB on the venv, see `intern.rs`), which are alive with the
  merged interner during `Interner::merge_all`, plus about one pending chunk per thread.
  `parse-only` peaks at 440 MB without building a global index (it keeps per-thread tables
  with doubling slack).

How the peak was brought down (venv, 14 threads, same session conditions):

| merge strategy                                                          | peak RSS MB |
|-------------------------------------------------------------------------|-------------|
| keep every chunk, then concatenate into exactly-sized tables             | 733         |
| append each chunk once all earlier ones are in (rayon range split)       | 554         |
| ... chunks handed out in FileId order from an atomic counter             | 502         |
| ... plus one reservation of the global tables from an early estimate     | 494-498     |

With rayon's recursive range splitting, threads work on chunks far apart, so almost every
chunk waited for the first half of the range. Handing chunks out in order bounds the wait
to about one chunk per thread. Without the reservation the global columns grow by doubling,
and macOS malloc keeps freed blocks up to 8 MiB resident. Reserving 1.5x the extrapolated
size once 1/16 of the chunks are in avoids most of that. Overshoot is untouched virtual
memory, which the final `shrink_to_fit` releases.

### Import resolution (S3.1)

`resolve` target: `load`, then `resolve_imports` (all three phases in parallel per file against
the read-only index). 14-core M4 Pro, release, warm cache, best of 3 (1 run for the venv at
one thread); load average about 5-7 from other worktrees. Only the `resolve.*` phases are
new; walk and load are as in the `load` rows above.

| corpus | threads | import rows | names (ModIds) | resolve.modules | resolve.imports | resolve.edges | resolve total | `Imports::heap_bytes` | peak RSS MB (load / resolve) |
|--------|---------|-------------|----------------|-----------------|-----------------|---------------|---------------|-----------------------|------------------------------|
| stdlib | 1       | 10,040      | 1,046          | 0.69ms          | 0.54ms          | 0.17ms        | 1.4ms         | 0.65 MB               | - / 20.6                     |
| stdlib | 14      | 10,040      | 1,046          | 0.32ms          | 0.43ms          | 0.40ms        | 1.15ms        | 0.65 MB               | 27.3 / 28.3                  |
| venv   | 1       | 901,141     | 70,121         | 28.6ms          | 52.7ms          | 9.92ms        | 91.2ms        | 48.3 MB               | - / 431.7                    |
| venv   | 14      | 901,141     | 70,121         | 3.99ms          | 6.79ms          | 1.53ms        | 12.3ms        | 48.3 MB               | 493.4 / 531.4                |

- **Time**: resolution is about 0.4% of the load on the venv (12ms against 3.1s at 14
  threads, 91ms against 11.6s at one thread): 13-14 ns per import row at 14 threads. Every
  lookup is a `u64` hash probe in the module trie or a binary search in one file's namespace
  run; no string is built per row.
- **Memory**: 48.3 MB on the venv = 2.8 B/LOC (index: 20.3 B/LOC). Phase results are 24 B
  per import row (21.6 MB), namespaces 12 B per module-level name, module edges and external
  edges 12 B each (329k and 174k), the module trie 17 B per name plus its hash map. Peak RSS
  grows by about 40 MB over `load`, i.e. the retained tables plus the per-chunk output
  vectors being concatenated.
- **Outcome shares** (venv, deps loaded as one root, attribution by the built-in stdlib table
  only): 64.2% resolved to a module/symbol/variable, 34.1% external (stdlib 291k rows,
  unknown 16k: optional dependencies that are not installed), 1.3% missing (mostly extension
  modules, which have no source: `MissingModule` 9.8k, `MissingName` 1.9k), 0.2% dynamic
  (`__getattr__`, `globals()`). On the stdlib corpus `missing_pm` is 13.5% because the
  corpus root includes `site-packages`, whose files are not importable from that root (a
  hyphen in the path); relative imports between non-importable files are reported missing.
  The stdlib alone (`stdlib_imports_resolve`, ignored test, 4,483 rows) leaves 1.3%
  unresolved: `idlelib.idle_test` (not shipped), PEP 695 `type` aliases (not extracted yet),
  platform-only modules (`win32evtlog`, `java.lang`) and `test.support`.

### Call resolution (S3.2)

`calls` target: `resolve`, then `resolve_calls`. 14-core M4 Pro, release, warm cache; 14
threads best of 3, one thread a single run; load average about 5-7 from other worktrees. Only
the `resolve.classes` / `resolve.mro` / `resolve.calls` phases are new. The `resolve` peak RSS
column is the `resolve` target (14 threads: measured alongside; one thread: the S3.1 numbers).

| corpus | threads | call rows | classes | resolve.classes | resolve.mro | resolve.calls | calls total | edges | `ResolvedCalls::heap_bytes` | peak RSS MB (resolve / calls) |
|--------|---------|-----------|---------|-----------------|-------------|---------------|-------------|-------|-----------------------------|-------------------------------|
| stdlib | 1       | 83,013    | 3,471   | 0.95ms          | 0.48ms      | 8.74ms        | 10.2ms      | 40,172 | 1.23 MB                    | 20.6 / 21.0                   |
| stdlib | 14      | 83,013    | 3,471   | 0.48ms          | 0.45ms      | 1.18ms        | 2.1ms       | 40,172 | 1.23 MB                    | 27.9 / 28.6                   |
| venv   | 1       | 3,496,139 | 159,758 | 33.1ms          | 25.1ms      | 437.5ms       | 496ms       | 2,232,016 | 60.0 MB                 | 431.7 / 518.3                 |
| venv   | 14      | 3,496,139 | 159,758 | 4.33ms          | 6.08ms      | 44.0ms        | 54.4ms      | 2,232,016 | 60.0 MB                 | 537.4 / 594.1                 |

- **Time**: 125 ns per call row on one core (the per-file locals table included), 13 ns at 14
  threads; call resolution is 1.7% of the end-to-end time on the venv (54ms of 3.2s). The old
  v2 took 13.3s single-threaded on the stdlib (linear scans, `format!` per lookup); this takes
  10ms. The MRO pass is sequential (6ms on the venv) and bounds the speedup at 14 threads.
- **Memory**: 60 MB on the venv = 3.5 B/LOC: edges are 16 B each (35.6 MB, 2.2M edges of which
  111k override and 14k decorator edges), one byte per call row (3.5 MB), class members 12 B
  each, the method index 8 B per method, MROs 4 B per entry. Per-file scratch (the locals
  table, 16 B per local binding) is reused per chunk of files.
- **Outcome shares** (per mille of call rows): stdlib corpus 311 certain, 43 likely, 35
  possible, 609 unresolved, of which 389 are builtins / builtin-type methods (`len`,
  `x.append`) and 73 unloaded code or unloaded bases; venv 416 / 45 / 13 / 524 with 262
  builtin and 81 external. The stdlib alone (`stdlib_calls_resolve`, ignored test, prints the
  reason histogram) leaves 5.8% ambiguous method names, 2.7% variables without a hint, 2.8%
  dynamic (star imports of C modules: `os.stat`), 1.9% unresolved imports (the
  non-importable `site-packages` files), 0.9% parameters.

### Graph (S3.3)

`graph` target: `calls`, then the call graph, its condensation and the module graphs. 14-core
M4 Pro, release, warm cache, best of 3 (RSS worst of 3), load average about 5-7 from other
worktrees. Everything in `pymapper_core::graph` is sequential (Tarjan is inherently so; the
counting-sort build is memory bound), so the thread count does not matter for the new phases.

| corpus | graph nodes | distinct edges | SCCs | cyclic SCCs | largest | DAG edges | graph.build | graph.scc | graph.modules | `callgraph_kb` | `graph_kb` | peak RSS MB (calls / graph) |
|--------|-------------|----------------|------|-------------|---------|-----------|-------------|-----------|---------------|----------------|------------|-----------------------------|
| stdlib | 25,441      | 31,873         | 24,624 | 564       | 94      | 29,429    | 0.71ms      | 1.34ms    | 0.68ms        | 1,315          | 1,473      | 28.9 / 29.9                 |
| venv   | 998,345     | 1,382,184      | 963,231 | 9,453    | 9,686   | 1,261,259 | 30.5ms      | 43.4ms    | 46.3ms        | 53,966         | 66,427     | 590.1 / 642.9               |

Module graphs: stdlib 1,046 modules, 3,974 import edges (9 import cycles), 3,446 module call
edges (5 cycles); venv 70,732 modules, 329,342 import edges (520 cycles), 215,974 module call
edges (469 cycles).

- **Time**: 120 ms for all graphs of the venv (3.5% of the 3.4s pipeline). `graph.build`
  turns 2.23M call edges into 1.38M distinct ones (parallel call sites merged) at 73M input
  edges/s. `graph.scc` is Tarjan plus the DAG build. `graph.modules` includes lifting
  2.2M call edges to modules (two node -> file -> module lookups per edge, twice).
- **Memory**: a graph is `8 (n + 1) + 10 m` bytes with labels (forward + reverse offsets and
  targets, one label byte per direction). The venv call graph is 22 MB, its condensation 32 MB
  (call graphs are nearly acyclic, so the DAG is almost as large as the graph; `Scc` is
  `8 n + 4 k`). Peak RSS grows by 53 MB over `calls`. The build needs no edge list: two passes
  over the source (count, scatter), then counting-sort transposes; before deduplication both
  directions hold every input edge.

Synthetic graphs (`cargo bench -p pymapper-core --bench graph -- <shape>`, 1M nodes, average
out-degree 3 unless noted; times best of 5, peak RSS from a separate single-rep run because
the allocator keeps freed pages across reps; peak RSS includes the generated edge list, 24-32
MB resident):

| shape   | input edges | distinct | SCCs | largest | build  | `Scc::of` | `Condensed::of` | graph MB | condensed MB | peak RSS MB |
|---------|-------------|----------|------|---------|--------|-----------|-----------------|----------|--------------|-------------|
| random  | 3,000,000   | 2,999,997 | 114,964 | 885,037 | 37.6ms | 62.1ms  | 90.7ms          | 32.0     | 10.5         | 83.3        |
| calls   | 3,000,000   | 2,965,149 | 172,243 | 827,540 | 36.9ms | 37.4ms  | 65.9ms          | 31.7     | 11.9         | 84.5        |
| chain   | 999,999     | 999,999  | 1,000,000 | 1     | 9.8ms  | 7.1ms     | 24.4ms          | 16.0     | 28.0         | 84.5        |
| cycle   | 1,000,000   | 1,000,000 | 1      | 1,000,000 | 9.7ms | 9.2ms    | 12.4ms          | 16.0     | 8.0          | 60.7        |
| blocks  | 1,399,998   | 1,399,998 | 200,000 | 5     | 17.8ms | 19.0ms    | 32.7ms          | 19.2     | 13.6         | 54.7        |
| random, degree 10 | 10,000,000 | 9,999,948 | 85 | 999,916 | 120.8ms | 128.0ms | 152.2ms   | -        | -            | -           |

`chain` is a million-deep DFS (no recursion, 8 bytes per DFS frame). Tarjan runs at 64-280M
nodes + edges per second; random graphs are the slow case (cache misses on `rindex`).

### Cache (S2.5)

`pymapper_core::cache::load` cold (cache cleared, every segment written) and warm (every file
a cache hit) against the uncached `load` target. 14-core M4 Pro, release, warm OS file cache,
best of 3 (RSS worst of 3), load average about 7 from other worktrees, so times are upper
bounds. The stdlib cache targets load the stdlib proper (632 files). The `load` target also
walks the base interpreter's `site-packages` (1043 files). The venv cache targets take file
lists from `RECORD` (phase `files`, after `discover`), so there is no walk column. The `inputs`
column is the walk, or `discover` + `files`.

| corpus | target      | threads | inputs  | load.extract | merge + build | cache.write | total   | peak RSS MB |
|--------|-------------|---------|---------|--------------|---------------|-------------|---------|-------------|
| stdlib | `load`      | 14      | 5.45ms  | 33.7ms       | 1.79ms        | -           | 41.3ms  | 27.6        |
| stdlib | `load-cold` | 14      | 3.07ms  | 21.3ms       | 1.27ms        | 2.94ms      | 29.0ms  | 22.5        |
| stdlib | `load-warm` | 14      | 1.20ms  | 2.48ms       | 1.33ms        | -           | 5.52ms  | 23.2        |
| stdlib | `load-cold` | 1       | 10.5ms  | 133.0ms      | 0.43ms        | 3.58ms      | 147.9ms | 17.4        |
| stdlib | `load-warm` | 1       | 1.84ms  | 3.94ms       | 0.42ms        | -           | 6.68ms  | 17.9        |
| venv   | `load`      | 14      | 922ms   | 2.17s        | 76.3ms        | -           | 3.18s   | 492.8       |
| venv   | `load-cold` | 14      | 51.2ms  | 2.28s        | 74.2ms        | 98.2ms      | 2.59s   | 505.8       |
| venv   | `load-warm` | 14      | 45.9ms  | 88.1ms       | 76.3ms        | 0.25ms      | 237.4ms | 498.8       |
| venv   | `load-cold` | 1       | 136ms   | 8.63s        | 45.0ms        | 319ms       | 9.22s   | 419.6       |
| venv   | `load-warm` | 1       | 147ms   | 299ms        | 45.3ms        | 0.27ms      | 521.6ms | 391.7       |

Cache sizes: stdlib 4.8 MB in one segment (15.7 B/LOC). Venv 284 MB in 491 segments (16.6
B/LOC; facts plus metrics are about 14.6 B/LOC in memory, and each segment adds its own string
table and paths).

- **Warm vs cold**: with 14 threads the venv loads 13x faster than the uncached `load`
  (3.18s to 237ms) and the stdlib 7.5x faster (41ms to 5.5ms). With one thread a warm load is
  18x faster than a cold one on the venv (9.22s to 522ms) and 22x on the stdlib. A warm
  `load.extract` is the checksum, validation and path index of each segment, plus a copy of
  the rows and one intern per distinct string per worker and segment. It runs at about 3.2
  GB/s of segment with 14 threads (0.95 GB/s with one). Most of what is left of a warm venv
  load is outside the cache: `discover` (reads every `RECORD`, 45ms), the interner merge
  (26ms) and the hierarchy (50ms).
- **Cold overhead**: `load.extract` also hashes every source and records its stamp (2.28s vs
  2.17s uncached, close to the noise at this load average). `cache.write` encodes and writes
  every segment in 98ms with 14 threads (4% of the total) or 319ms with one (about 0.9 GB/s
  per core). The cold venv total (2.59s) is below the uncached `load` (3.18s) only because it
  reads `RECORD`s instead of walking the venv.
- **Memory**: warm and cold peak RSS stay within about 3% of the uncached load (the index
  itself is 346 MB). A segment is mapped (or read, under 64 KiB) by the first worker that
  needs it and unmapped by the last one done with its files, so only the segments being
  copied are resident, not the whole 284 MB cache. Segments are encoded in parallel and
  streamed to disk one section at a time (memory: the largest section, not the file).
- **First-party files** are checked with one `stat` each (size + mtime). A file whose mtime
  changed but whose size did not is read and hashed, and is reused if the content is the same
  (counter `verified`). A file written within 2s of caching is stored with an unknown mtime,
  so the next load checks its content.

### Output (S4.4)

Sorting, every format and the baseline round trip on synthetic findings (`cargo bench -p
pymapper-core --bench output -- [N] [FILES]`): three rules (one with a symbol and two numbers,
one with a symbol, one with a name), resolver backed by pre-rendered tables, output to a
byte-counting sink. 14-core M4 Pro, release, one thread (output is sequential). Other
worktrees were building at the same time, so times are upper bounds.

| findings / files | push order       | sort  | concise | json  | sarif | github | baseline save / load / apply |
|------------------|------------------|-------|---------|-------|-------|--------|------------------------------|
| 10k / 500        | scrambled        | 54ns  | 94ns    | 213ns | 225ns | 140ns  | 170ns / 100ns / 37ns         |
| 1M / 10k         | file-interleaved | 68ns  | 94ns    | 207ns | 225ns | 138ns  | 227ns / 119ns / 153ns        |
| 1M / 10k         | scrambled        | 114ns | 321ns   | 455ns | 462ns | 377ns  | 344ns / 118ns / 226ns        |

Times are per finding (per entry for load). Line sizes: concise 97 B, json 233 B, sarif 452 B,
github 143 B per finding; the baseline file is 80 B per key.

- **Throughput**: about 1 GB/s of concise or JSON output and 1.9 GB/s of SARIF when findings
  arrive roughly in file order, which is how per-file analyses produce them. Fully scrambled
  1M-finding buffers are 3x slower: the sorted walk reads findings, their arguments and the
  name tables at random (cache misses), not formatting.
- **Allocation**: none per finding. `Report::new` allocates its index array and one path pool
  (each distinct path rendered once); writing allocates the 64 KiB output buffer and three
  512-byte scratch buffers, the same count at 1k and 50k findings (`tests/output_alloc.rs`
  checks this with a counting allocator).
- **Memory**: 4 B per finding for the order plus the distinct paths. A loaded baseline holds
  one byte pool with every key (rule + length-prefixed id and detail parts) plus about 32 B
  per key for the entry, the hash index and the collision chain.
