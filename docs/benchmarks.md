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
| `extract-mem` | `walk`, `load`, `parse` | the extractor micro benchmark: `parse-only`'s front end from memory (no I/O in `parse`). `XTASK_EXTRACT_STAGES=parse\|metrics\|facts-spans\|facts\|all` runs only part of it, to cost each stage; `XTASK_EXTRACT_REPS=N` repeats the in-memory pass N times and reports the fastest (peak RSS then holds two passes). On macOS it adds `parse_kinstr` / `parse_kcycles`: instructions and CPU cycles (thousands) of the fastest-cycles pass, from `proc_pid_rusage` |
| `roots`      | `discover`, `module-map` | `pymapper_core::roots::discover` (deps + stdlib on) for the venv enclosing the corpus, then the module map over every `RECORD` file |
| `load`       | `walk`, `load.extract`, `load.merge`, `load.build` | end-to-end `pymapper_core::index::load` with the corpus root as the import root of one group: parallel read -> extract -> intern -> drop source, in-order merge into the global tables, interner merge + remap, hierarchy. The whole index is alive at the end (counters `loc`, `defs`, `nodes`, `strings`, `index_kb` = `Index::heap_bytes`, `facts_kb`, `metrics_kb`, `intern_kb`, `index_B_per_kLOC`) |
| `dupes`      | `walk`, `load.*`, `dupes.seeds`, `dupes.relex`, `dupes.extend`, `dupes.locate`, `dupes.group` | `load` with duplication fingerprints taken in the extraction pass (`index::load_with`, every file first-party), then `analysis::dupes::analyse` with the default thresholds. Counters: `load_peak_rss_mb` (peak RSS before matching: matching's own cost is the run's peak minus this), `fingerprints`, `fingerprints_kb`, `seeds` (= `extended` + `covered` + `unverified`: seeds that failed verification, e.g. after a parse error shifted the token stream), `tokens_cached` (tokens held while extending, 2 B each), `files_reread`, `bytes_reread_kb`, `pairs`, `copies`, `groups`, `duplicated_lines`, `findings` |
| `dupes-cold` | `walk`, `load.*`, `cache.write`, `dupes.*` | `dupes` through `cache::load_with` after clearing the cache (in `$TMPDIR/pymapper-bench-cache/<corpus hash>/dupes`): fingerprints are stored with the facts of the one first-party segment. Extra counters `hits`, `misses`, `written_kb` |
| `dupes-warm` | same as `dupes-cold`   | like `dupes-cold` but keeps the cache: after the warm-up run every file is a hit and gets its stored fingerprints; finds the same clones as `dupes` |
| `resolve`    | `walk`, `load.*`, `resolve.modules`, `resolve.imports`, `resolve.edges` | `load`, then `pymapper_core::resolve::resolve_imports` (module table + namespaces, per-row resolution, module edges) with the built-in stdlib table as the only attribution. Counters `imports`, `modules`, shares of import rows in per mille (`found_pm`, `dynamic_pm`, `missing_pm`, `external_pm`), `module_edges`, `ext_edges`, `resolve_kb` = `Imports::heap_bytes`, `index_kb` |
| `calls`      | `walk`, `load.*`, `resolve.modules`, `resolve.imports`, `resolve.edges`, `resolve.classes`, `resolve.mro`, `resolve.calls` | `resolve`, then `pymapper_core::resolve::resolve_calls` (class table + method index, bases + C3 MROs, then every call row, uncalled decorator and override in parallel per file). Counters `calls`, shares of call rows in per mille (`certain_pm`, `likely_pm`, `possible_pm`, `unresolved_pm`; of which `builtin_pm` = builtins and builtin-type methods, `external_pm` = unloaded code and unloaded bases), `edges`, `override_edges`, `decorator_edges`, `classes`, `incomplete_classes`, `calls_kb` = `ResolvedCalls::heap_bytes`, `resolve_kb`, `index_kb` |
| `graph`      | `calls` phases, `graph.build`, `graph.scc`, `graph.modules` | `calls`, then `pymapper_core::graph`: `graph.build` = `call_graph` over every node (all edges with a target), `graph.scc` = its `Condensed` (iterative Tarjan + DAG), `graph.modules` = `import_graph` (no flag mask) + `module_call_graph` and both condensations. Counters `graph_nodes`, `graph_edges` (distinct), `sccs`, `cyclic_sccs`, `largest_scc`, `dag_edges`, `modules`, `import_edges`, `import_cycles` and `module_call_cycles` (components with 2+ modules), `module_call_edges`, `callgraph_kb` (call graph + condensation heap), `graph_kb` (all six structures), `calls_kb`, `index_kb` |
| `reach`      | `calls` phases, `graph.build`, `graph.scc`, `reach.roots`, `reach.forward`, `reach.weights`, `reach.sizes` (+ `reach.exact`) | `graph`'s call graph and condensation, then `pymapper_core::reach`: application-mode roots, BFS from them, index weights, HLL reach sizes per node. `XTASK_REACH_P` sets the precision (default 10), `XTASK_REACH_EXACT=1` adds the exact sizes and their comparison (counters `estimated_sets`, `rmse_ppm`, `worst_ppm`), `XTASK_REACH_REPS=N` reruns the sizes N times (`sizes_best_us`). Counters `roots`, `live`, `defs`, `live_defs`, `max_symbols_reached`, `max_lines_reached`, `dense_sets`, `peak_live_sets`, `peak_dense`, `peak_dense_kb`, `peak_sparse_kb`, `reach_kb` (roots + reached + weights + sizes), `callgraph_kb`, `index_kb` |
| `cycles`     | `resolve` phases, `cycles`, `layers`, `layers.transitive` | `resolve`, then `analysis::cycles::analyse` (`cycles`: first-party runtime import graph, SCCs, witnesses, PM301 findings) and `analysis::layers::analyse` with four synthetic contracts over the corpus' top-level packages (alphabetical): 6 layers, 12 independent modules, up to 32 sources forbidden from `os`, `sys`, `typing`, and every top-level package and module forbidden from them (the worst case: every import row is checked) (phase `layers`), then the same contracts with `transitive = true` (phase `layers.transitive`). Counters `modules`, `import_edges` (of the cycle graph), `cycles`, `modules_in_cycles`, `largest_cycle`, `longest_witness`, `cycle_findings`, `contracts`, `missing_names`, `rows_checked`, `layer_findings`, `transitive_findings`, `cycles_kb` = `Cycles::heap_bytes`, `layers_kb` / `layers_transitive_kb` = `LayersReport::heap_bytes`, `index_kb` |
| `rollup`     | `calls` phases, `rollup.build`, `rollup.aggregates`, `rollup.levels` | `calls`, then `pymapper_core::rollup`: `rollup.build` = `Rollup::build` (all call edges with a target, all import edges), `rollup.aggregates` = `Aggregates::build`, `rollup.levels` = `Rollup::level_with` for every `Level::Depth(k)` from 0 to the deepest node, then `Level::Module`, with one reused scratch and output buffer. Counters `total_loc` (aggregated LOC of the distributions), `nodes`, `call_edges`, `call_pairs`, `sites`, `multi_sites`, `import_edges`, `levels`, `rollup_kb` (base tables), `agg_kb`, `scratch_kb`, `level_max_kb` (output buffer at its largest), `calls_kb`, `index_kb`, and per level `d{k}_edges` / `d{k}_us`, `module_edges` / `module_us` |
| `hotspot`    | `load` phases, `hotspot.scan`, `hotspot.top` | `load`, then `pymapper_core::analysis::hotspot::analyse` with the default config (`hotspot.scan`: PM101-PM107 findings plus the per-module aggregates) and `top_functions` for the 100 worst functions by cognitive complexity (`hotspot.top`). Counters `loc`, `functions`, `classes`, `flagged` (functions over any function threshold), `findings`, `top_len`, `top_cognitive`, `hotspot_kb` = `ModuleHotspots::heap_bytes`, `index_kb` |
| `coupling`   | `calls` phases, `graph.build`, `coupling.analyse` | `calls`, then the call graph over every node (all edges with a target) and `pymapper_core::analysis::coupling::analyse` with the default config (module Ca/Ce, abstractness, PM201-PM204). Counters `loc`, `modules` (files), `import_edges` (counted edges = sum of Ce), `class_candidates` (classes above `max-class-wmc`), `findings`, `coupling_kb` = `ModuleCoupling::heap_bytes`, `callgraph_kb`, `calls_kb`, `index_kb` |
| `deadcode`   | `calls` phases, `deadcode.roots`, `deadcode.graph`, `deadcode.propagate`, `deadcode.evidence`, `deadcode.findings` | `calls`, then `pymapper_core::analysis::deadcode::analyse` with every PM4xx rule and the name fallback, in application mode with no entrypoints (the fewest roots, so the most candidates; `XTASK_DEADCODE_MODE=library` or `auto` changes the mode): roots, the call and override graphs plus seeds, the level propagation of defs and modules (module graphs included), the evidence scan plus the second propagation, the findings. Counters `loc`, `nodes`, `library`, `roots`, `live` (nodes at level 100), `candidates` (defs, modules and imports probed by the name fallback), `probes` (evidence hashes scanned), `expansions`, `pm401`..`pm405`, `findings` (emitted, confidence >= 10), `reported` (confidence >= 80), `graph_kb` (call + override graphs and seeds), `deadcode_kb` = `DeadCode::heap_bytes`, `calls_kb`, `index_kb` |
| `query`      | `calls` phases, `query.build`, `query.names`, `query.imports`, `query.cycle_graph`, then one `query.<kind>` phase per batch | `calls`, then `pymapper_core::query`: `query.build` = `Query::new` (the labelled call graph), `query.names` = the name index, `query.imports` = the import graph, `query.cycle_graph` = the cycle import graph and its SCCs (all lazy in the API, forced here), then batches over up to 1000 evenly spaced defs and 200 packages/modules with one reused `Scratch`: `lookup_exact` (each def's qid), `lookup_name` (its last segment), `lookup_suffix2` (last two segments), `lookup_fuzzy` (the name minus its first byte, 100 samples), `callers1`, `callees3` (depth 3), `callers_all` (unbounded), `impact`, `path` (from the deepest transitive caller of each def to the def: a path exists), `path_random` (def `i` to def `n - 1 - i`: mostly no path), `deps_out` / `deps_in` (module level), `import_cycles`, `call_cycle`, `summary` (once). Counters `loc`, `nodes`, `call_edges`, `query_kb` (`Query::heap_bytes`), `names_kb`, `calls_kb`, `index_kb`, per kind `<kind>_queries`, `<kind>_avg_ns`, `<kind>_max_us`, `<kind>_avg_items` (mean result size), and `scratch_kb` |
| `load-cold`  | inputs, `load.extract`, `load.merge`, `load.build`, `cache.write` | `pymapper_core::cache::load` after clearing the cache: the `load` pipeline plus encoding and atomically writing every segment. Inputs are built the way the CLI builds them: a venv's site-packages through import roots (phases `discover`, `files`: file lists from `RECORD`, no walk of distribution files; one trusted segment per distribution), a stdlib through a walk without the base interpreter's `site-packages` plus import roots (one trusted segment per interpreter), anything else as one first-party group validated per file (`walk`). The cache lives in `$TMPDIR/pymapper-bench-cache/<corpus hash>`. Counters: `segments`, `opened`, `hits`, `verified`, `misses`, `written`, `opened_kb`, `written_kb`, `index_kb` |
| `load-warm`  | same as `load-cold`    | like `load-cold` but keeps the cache, so after the warm-up run every file is a cache hit (no read, no parse; first-party files cost one `stat` each) |
| `incremental` | `walk`, `copy`, `load.*`, `resolve.*`, `incremental.update` | single-file edit latency of `pymapper_core::incremental::Program::update`: the corpus is copied to `$TMPDIR` (`copy`; corpora over 200 MB are not copied and their files are re-extracted with unchanged contents instead, counter `inplace`), loaded and resolved with dependency tracking (`load.*`, `resolve.*`), then 6 files across the size distribution (plus `typing.py`, `os.py`, `collections/__init__.py`, `functools.py`, `re/__init__.py` when present) are each edited (a function appended) and restored, one update per edit, 3 rounds. `incremental.update` is the sum of all updates. Counters: `updates`, `update_median_us` / `update_best_us` / `update_worst_us`, `hot_median_us` (the hot stdlib modules), per-phase medians `plan_us`, `extract_us`, `patch_us` (splice + hierarchy + node map), `resolve_imports_us`, `resolve_calls_us`, `diff_us`, `reresolved_imports_median` / `reresolved_calls_median` (files resolved again per update), `deps_imports` / `deps_calls` (recorded (file, file read) pairs), `tracking_kb`, `heap_kb_before` / `heap_kb_after` (`Program::heap_bytes` before and after the updates) |

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

### Front-end speed (S1.6)

Goal: move the front end towards 400 MB/s/core without changing any result. 14-core M4 Pro,
release build. The machine was shared with other worktrees the whole time (load average 5 to
40), so single-process best-of-N wall times moved by up to 10% between runs. Every increment
was therefore judged by an **interleaved A/B**: the old and new binaries alternate, each run
does `XTASK_EXTRACT_REPS=10` in-memory passes of `extract-mem -j 1` on the stdlib, and the
fewest CPU cycles of one pass (`parse_kcycles`, from `proc_pid_rusage`) is compared. Cycles
only count time on a core and repeat within about 0.3%. A change was kept only if it saved
more than about 1.5% of cycles.

Equivalence: `xtask facts-digest DIR` hashes, for every file, each fact column (evidence
hashes too), the rendered facts with resolved names, the interned strings, every metrics row,
and the lexer's token streams (parser mode and trivia mode) plus suppressions. The digests
were taken before this step and still match after every kept change:

| corpus | files  | digest             |
|--------|--------|--------------------|
| tests/corpus | 29 | `db9257ab008f0205` |
| stdlib | 1,043  | `e9853f071b4e8acf` |
| venv   | 60,529 | `cdbc7e7ace54d802` |

`diff-ruff` still finds 0 mismatches on tests/corpus, the stdlib and the venv (927,531 defs,
901,139 imports, 3,496,134 calls). `xtask fuzz` ran 300 s per target with 0 crashes and 0
hangs: `extract` 291M inputs, `lex` 1,096M inputs.

Increments (stdlib, `extract-mem -j 1`; cycles and instructions of one pass, change against
the build before the step; "kept" means the change is in the tree):

| increment | cycles | instructions | verdict |
|-----------|--------|--------------|---------|
| line index: the parser's forward line cursor (a SWAR newline count per query) replaced by a line-start index built in one NEON pass (SSE2 on x86_64, SWAR elsewhere) in a per-thread reused buffer; lookups walk from the last line, binary search for long jumps | -2.1% | -3.9% | kept |
| docstring line count by a vectorised `\n` count instead of a byte loop with lookahead; suppression lines from the parser's line index instead of a second byte-wise scan in the fact builder | -1.7% | -2.5% | kept |
| string evidence: one table-driven pass classifies a literal as path / type expression / other, instead of up to four (`:` count, split + validate, `[`/`\|` search, charset check) | -1.7% | -2.1% | kept |
| hash each identifier once: the lexer hashes every `Name`, the parser passes the hash with `token`/`name`/`attribute` events, the fact builder reuses it for evidence and (through a 64-slot cache by token start) for interning, metrics for Halstead operands | +1.1% | +6.6% | dropped |
| same, hashed lazily by the parser on first use instead of in the lexer | +2.5% | +7.3% | dropped |
| line index filled branch-free (count first, two unconditional stores per 16-byte block) | -0.2% | +5.3% | dropped |
| parser ring refill (`fill`) out of line | +1.4% | +5.3% | dropped |
| metrics: operator kinds gathered in one register-like field and folded into the frame on scope changes; docstring tracking skipped by a flag | -0.4% | -1.1% | dropped |
| interner: 512-entry direct-mapped cache of recent strings in front of the index | +0.6% (venv +0.6%) | -0.2% | dropped |
| lexer: one "special work" flag instead of testing four state fields per token | -1.3% (parse stage +2.0%) | -3.7% | dropped (not consistent) |

Hashing once did not pay. Most likely it was never three hashes in practice: both visitors
are inlined into the parser through `Tee`, so the evidence and Halstead hashes of the same
token can be one computation already (not verified in the assembly), and hashing every `Name`
up front (lookahead rescans included) plus carrying the hash through the ring buffer and the
events costs more than the interner hashes it saves. SIMD went only where the profile showed a hot scalar
loop: the newline scan. The identifier scan (`skip_ident`, about 1.2% of samples), the
indentation scan (`line_start`, 1.5%) and comment/string body searches (`find2`/`find4`,
under 1%) are already word-at-a-time and too short per call (identifiers average under 8
bytes) for 16-byte vectors to help.

Before / after (interleaved A/B of the binaries before and after this step, `extract-mem -j 1`,
in-memory passes; best of 6 alternations for the stdlib `all` row, 2 for the stage rows and
the venv):

| corpus | stages (`XTASK_EXTRACT_STAGES`) | cycles before | cycles after | change | cycles/byte after | MB/s/core before -> after |
|--------|-----------------|---------|---------|-------|-------|-----------|
| stdlib | `parse` (lexer + parser) | 167.1M | 158.8M | -4.9% | 10.07 | 369 -> 384 |
| stdlib | `metrics`        | 231.9M  | 219.0M  | -5.6% | 13.89 | 272 -> 291 |
| stdlib | `facts-spans`    | 220.5M  | 209.4M  | -5.0% | 13.28 | 285 -> 300 |
| stdlib | `facts`          | 245.2M  | 230.5M  | -6.0% | 14.62 | 256 -> 271 |
| stdlib | `all` (the real front end) | 305.4M | 286.5M | -6.2% | 18.17 | 206 -> 222 |
| venv   | `all`            | 12,098M | 11,307M | -6.5% | 17.39 | 200 -> 225 |

Instructions dropped 8.3% (stdlib) and 10.1% (venv). The clock ran at about 3.9-4.0 GHz, so
400 MB/s/core needs about 9.8 cycles per byte; the lexer + parser with a no-op visitor alone
take 10.1.

Final numbers, standard `cargo xtask bench` (single-process best of N; load average 14 to 33
during these runs, so they are lower than the A/B passes above):

| corpus | target        | threads | walk    | load / read+parse | parse phase | total   | MB/s/core | peak RSS MB |
|--------|---------------|---------|---------|-------------------|-------------|---------|-----------|-------------|
| stdlib | `extract-mem` | 1       | 16.3ms  | 99.1ms            | 78.6ms      | 195.8ms | 197       | 36.2        |
| stdlib | `extract-mem` | 14      | 5.92ms  | 33.8ms            | 9.54ms      | 50.2ms  | 134       | 40.3        |
| stdlib | `parse-only`  | 1       | 12.7ms  | 166.4ms           | -           | 179.1ms | 186       | 20.1        |
| stdlib | `parse-only`  | 14      | 6.12ms  | 45.3ms            | -           | 51.7ms  | 166       | 23.6        |
| venv   | `extract-mem` | 1       | 2.67s   | 5.47s             | 2.76s       | 11.21s  | 229       | 991.8       |
| venv   | `extract-mem` | 14      | 993ms   | 2.21s             | 292ms       | 3.62s   | 130       | 1240.0      |
| venv   | `parse-only`  | 1       | 2.71s   | 9.12s             | -           | 12.03s  | 200       | 351.1       |
| venv   | `parse-only`  | 14      | 891ms   | 2.81s             | -           | 3.70s   | 167       | 452.1       |

On 14 threads the venv front end extracts 650 MB in 292ms from memory (2.2 GB/s); MB/s/core
there averages performance and efficiency cores competing with the other worktrees' load.

Memory is unchanged: facts are identical, so retained bytes per LOC stay at 13.75 (facts) +
1.47 (metrics) = 15.22 on the venv and 12.96 + 1.51 = 14.46 on the stdlib. (The 14.95 B/LOC
of S1.5 above predates the `doc_lines` metrics column, 4 B per def, added after S1.5.) The
line index adds one per-thread buffer of 4 bytes per line of the largest file seen (buffers
above 4 MiB are not kept).

Verdict: **400 MB/s/core is not reached.** The full front end went from about 206 to 222
MB/s/core on the stdlib and from 200 to 225 on the venv (6-6.5% fewer cycles, 8-10% fewer
instructions). The profile stays flat: lexer about a quarter, parser and expression scanner
about a fifth, metrics about 12%, interning about 7% (mostly probing a large per-thread
table), fact building about 10%; no function is above 6% self time. Reaching 400 would need
the whole extractor to cost what the lexer + parser alone cost today (about 10 cycles per
byte, 77 per token), i.e. halving the per-token work of every stage, not removing a hot
spot. Candidates for a redesign rather than tuning: a lexer emitting tokens into a small
batch buffer that the parser consumes without per-token state reloads, and folding the
metrics token pass into the parser's own dispatch.

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

### Reachability (S3.5)

`reach` target: `graph`'s call graph and condensation, then `pymapper_core::reach`:
application-mode roots (`reach.roots`), BFS from them (`reach.forward`), index weights
(`reach.weights`) and HLL reach sizes per node at `p = 10` (`reach.sizes`). 14-core M4 Pro,
release, warm cache, best of 3 (RSS worst of 3), load average about 7-9 from other worktrees.
The corpus is one first-party group, so roots are every module scope plus `__main__` modules
and tests.

| corpus | graph nodes | DAG edges | roots | live (defs) | reach.roots | reach.forward | reach.weights | reach.sizes | dense sets | peak dense / sparse | peak RSS MB (graph / reach) |
|--------|-------------|-----------|-------|-------------|-------------|---------------|---------------|-------------|------------|---------------------|-----------------------------|
| stdlib | 25,441      | 29,429    | 1,043 | 5,311 (4,268 of 24,394) | 0.11ms | 0.09ms   | 0.05ms        | 7.6ms       | 734        | 118 KB / 140 KB     | 30.8 / 31.1                 |
| venv   | 998,345     | 1,261,259 | 167,516 | 274,835 (214,306 of 927,612) | 8.8ms | 3.5ms | 2.1ms       | 249ms       | 134,877    | 3.4 MB / 0.4 MB     | 650.7 / 667.3               |

- **Time**: roots, BFS and weights are O(n) passes (BFS at ~600M nodes + edges/s). The sketch
  propagation is single-threaded and is the largest reach phase: 249ms on the venv (6% of
  the pipeline). Most of it is unions of sparse node sets and inserting the line keys of
  nodes when a sparse set meets a dense one. Tuning on the venv (best of 5, `XTASK_REACH_REPS`):
  a sparse limit of `2^p / 4` nodes and `4 * 2^p` lines took 233ms, against 265ms at `2^p`
  lines, 303ms at `2^p / 4` lines and 348ms with no line limit. An earlier version that kept
  sparse sets as line keys took 528ms.
- **Memory**: output 8 B per node (`ReachSizes`), weights 4.1 B per node, roots 1 B per node,
  `Reached` 4.1 B per node: `reach_kb` = 17.2 MB on the venv. Live sets peaked at 15,920
  (1,478 dense buffers of 2.3 KB, 0.4 MB of sparse sets). Peak RSS grows by 17 MB over
  `graph`.
- **Precision** (venv, sizes above `2^p / 4` against `XTASK_REACH_EXACT=1`):

| p  | reach.sizes | RMSE   | worst | sizes compared | peak dense |
|----|-------------|--------|-------|----------------|------------|
| 8  | 186ms       | 7.8%   | 37%   | 530,975        | 1.6 MB     |
| 10 | 262ms       | 2.9%   | 16%   | 374,876        | 3.4 MB     |
| 12 | 511ms       | 1.0%   | 5.7%  | 286,708        | 6.2 MB     |

  RMSE matches `1.04 / sqrt(2^p)` (6.5% / 3.25% / 1.63%). The worst errors are 5 standard
  errors: a cluster of callers that share one subset whose estimate is off. A fresh sketch of
  the worst node's reach set gives the same estimate, so propagation loses nothing.
- **Exact mode** (`SizeMode::Exact`, gated): 47-55ms on 14 threads and 324ms on one thread for
  the venv, whose reach sets are small (at most 13,762 symbols). It is quadratic on graphs
  where many nodes reach a large common part (below).

Synthetic graphs (`cargo bench -p pymapper-core --bench reach -- <shape> [--exact]`, 1M nodes,
average out-degree 3, every node a symbol with 1-30 lines, BFS from every 100th node; best of
3, `p = 10`):

| shape  | SCCs    | DAG edges | forward | approx sizes | dense sets | peak dense | exact sizes (14 threads) | RMSE / worst |
|--------|---------|-----------|---------|--------------|------------|------------|--------------------------|--------------|
| calls  | 172,243 | 225,242   | 24.2ms  | 61.1ms       | 109,875    | 14 buffers | 2,196ms (36x)            | 2.4% / 3.8%  |
| random | 114,964 | 140,623   | 31.4ms  | 43.9ms       | 55,754     | 5.6 MB     | 1,382ms (31x)            | 2.7% / 3.8%  |
| chain  | 1,000,000 | 999,999 | 3.9ms   | 126ms        | 999,744    | 1 buffer   | not run (quadratic); 30k nodes: 196ms (75x) | 2.6% / 9.0% (30k) |
| blocks | 200,000 | 399,988   | 3.3ms   | 724ms        | 198,803    | 142 MB     | 100k nodes: 3.9ms (0.05x) | 2.2% / 9.4% (100k) |

`blocks` (5-node cycles with random forward edges to any later block) is the adversarial
case for the sketches: almost every set has a predecessor far ahead in the order, so 62,593
dense buffers are live at once (peak RSS 243 MB), while its reach sets are small enough
(at most 11.5k nodes) for the exact mode to be faster. `chain` is the opposite: every set is
stolen by its only predecessor (one live buffer) but exact search is quadratic.

### Rollup (S3.4)

`rollup` target: `calls`, then the rollup base tables, the node aggregates and every level.
14-core M4 Pro, release, warm cache, best of 3 (RSS worst of 3), load average about 5-6 from
other worktrees. Everything in `pymapper_core::rollup` is sequential. The corpus is loaded as
one group, so depth 0 (the one distribution) has no edges and depth 1 is the top-level
packages and modules.

| corpus | nodes   | call edges | call pairs | multi-target sites | import edges | rollup.build | rollup.aggregates | rollup.levels (levels) | `rollup_kb` | `agg_kb` | `scratch_kb` | peak RSS MB (calls / rollup) |
|--------|---------|------------|------------|--------------------|--------------|--------------|-------------------|------------------------|-------------|----------|--------------|------------------------------|
| stdlib | 25,441  | 40,172     | 31,873     | 4,329 of 32,906    | 3,974        | 0.77ms       | 0.18ms            | 2.47ms (12)            | 517         | 508      | 203          | 30.6 (`graph`) / 31.6        |
| venv   | 998,345 | 2,232,016  | 1,382,184  | 430,631 of 1,679,242 | 329,342    | 43.1ms       | 7.55ms            | 278.9ms (26)           | 29,194      | 19,966   | 7,987        | 592.5 / 686.8                |

Per level on the venv (edges, time with a warm scratch):

| level       | d0    | d1     | d2     | d3      | d4      | d5        | d6        | d8        | d12       | d24 (deepest) | module  |
|-------------|-------|--------|--------|---------|---------|-----------|-----------|-----------|-----------|---------------|---------|
| edges       | 0     | 3,200  | 46,048 | 230,984 | 619,880 | 1,071,291 | 1,340,124 | 1,506,819 | 1,661,523 | 1,707,326     | 437,739 |
| time        | 2.9ms | 4.5ms  | 7.2ms  | 10.1ms  | 12.6ms  | 12.7ms    | 12.3ms    | 11.6ms    | 11.4ms    | 11.3ms        | 10.5ms  |

- **Time**: a level costs 3-13 ms on the venv (17M LOC), so the GUI can re-query on every
  zoom step without caching levels. It is O(n + m): the node map (one forward pass over 1M
  nodes, the whole 2.9 ms of depth 0), then one pass over 1.71M base rows with a slot-array
  merge per source and a small sort per source. There is no global sort because the level maps
  are monotone in pre-order. Building the base costs 43 ms: two passes over the 2.2M call
  edges plus per-source bucket sorts (a counting sort by source, no comparison sort of the
  edge list). The aggregates are two passes over the nodes.
- **Memory**: the base is 29 MB on the venv (12 B per call pair, 12 B per import edge, 12 B per
  multi-target site plus 4 B per target). The aggregates are 20 B per node (20 MB), the level
  scratch 8 B per node (8 MB). A level is 24 B per edge: the deepest venv level is 41 MB (the
  output buffer, grown by doubling, peaked at 50 MB). Peak RSS grows 94 MB over `calls`, which
  is all of the above kept alive to the end. The transient build buffers (4 B per node plus 8 B
  per kept edge, 22 MB) are freed before the import pairs are built.
- **Sites**: 26% of the venv's call sites have several targets (mostly `Foo()` linking the
  class and its `__init__`). The level subtracts the extra counts per destination, so `sites`
  counts distinct call sites and not edges.
- Correctness: `stdlib_rollup_matches_reference` (ignored test) checks every level, under three
  call/import filters, against a hash-set brute force, and the aggregates against subtree walks.

### Treemap layout (S7.3)

`cargo bench -p pymapper-core --bench layout -- [--third DIR] [--stdlib DIR] [--first DIR]
[--synthetic NODES] [--reps R]`. `--third` makes every top-level entry of a `site-packages`
one distribution. Loading is not part of the layout numbers. Times are best of R, release
build, Apple M4 Pro (14 cores).

| input                                        | nodes     | levels | tree build | all levels | per node | rects   | tree    | cache save / load  |
|----------------------------------------------|-----------|--------|------------|------------|----------|---------|---------|--------------------|
| stdlib 3.13 (631 files, 305k LOC)            | 18 779    | 7      | 0.31 ms    | 0.43 ms    | 22.7 ns  | 0.3 MB  | 0.3 MB  | 0.71 / 0.10 ms     |
| venv + stdlib + crates/ (55k files, 16.9M LOC, 482 groups) | 912 128 | 25 | 25.8 ms | 5.8 ms | 6.4 ns | 14.6 MB | 16.4 MB | 7.4 / 4.5 ms |
| synthetic venv-like, 14 threads              | 2 001 421 | 5      | 24.3 ms    | 6.9 ms     | 3.4 ns   | 32.0 MB | 36.0 MB | 16.1 / 11.7 ms     |
| synthetic venv-like, 1 thread                | 2 001 421 | 5      | 23.9 ms    | 42.0 ms    | 21.0 ns  | 32.0 MB | 36.0 MB |                    |

Observations:

- The layout itself is a few milliseconds even for the whole venv; the deepest level a
  semantic zoom asks for is computed on the layout thread in well under a frame. The
  largest venv level (226k nodes at depth 5) takes about 1 ms.
- `LayoutTree::from_index` (25.8 ms on the venv) is dominated by hashing every node name for
  the hierarchy hash; it runs once per index.
- Memory is 16 B per laid-out node plus 18 B per node for the tree. Peak RSS (460 MB on the
  venv) is the index, not the layout.
- Loading the cache costs about as much as recomputing on these inputs (4.5 ms against
  5.8 ms on the venv). The layout is deterministic, so positions are stable across runs
  either way; the cache (keyed by hierarchy hash and config) only pays off if the layout gets
  costlier. It is written on the layout thread when idle, so it never delays a frame.

### Code map (S7.4)

`cargo run --release -p pymapper-gui --example codemap_bench -- PATH [--deps] [--isolated]
[--cache-dir DIR]`: loads the project in-process, builds the code map (layout, semantic-zoom
reveal bands, edge table) and its scenes, then renders offscreen at 2880x1800 px (2x) and
waits for the GPU after every frame. Release build, Apple M4 Pro (Metal), warm index cache.
Details and scenario tables: [docs/gui.md](gui.md#results-s74).

| input                                    | nodes     | load    | code map build | edges (bands) | scene build | fit frame p50 | zoom sweep p95 / max | code map + scene heap | peak RSS |
|------------------------------------------|-----------|---------|----------------|---------------|-------------|---------------|----------------------|-----------------------|----------|
| aiml-lectio + venv (`--deps`, 62k files) | 1 023 762 | 679 ms  | 88 ms          | 91 266 (16)   | 92-107 ms   | 1.51 ms       | 5.5 / 10.8 ms        | 38 + 100 MiB          | 1.1 GiB  |
| aiml-lectio first party                  | 24 866    | 191 ms  | 7.7 ms         | 70 325 (11)   | 4-14 ms     | 1.43 ms       | 3.2 / 5.7 ms         | 3.3 + 16.6 MiB        | 180 MiB  |
| stdlib 3.13 as a project                 | 18 755    | 73 ms   | 8.0 ms         | 48 078 (10)   | 1.4-6.2 ms  | 1.20 ms       | 4.0 / 7.3 ms         | 2.3 + 6.9 MiB         | 81 MiB   |

Observations:

- Edges for all 16 bands of the venv take 44 ms (4 workers, one rollup pass and one call-pair
  pass per band). Stopping edges into external code at depth 1 keeps the venv's table at
  91k edges; the first-party map alone has 70k.
- A scene build (100 ms on the venv: 1.38M rect instances, cells plus label carriers, and
  380k curve segments, sorted into grid order) runs on the map thread, never in a frame. It
  has not been profiled further.
- Peak RSS on the venv is the index (about 660 MiB after loading) plus a transient second
  scene while a new one is built.

### Hotspots and coupling (S4.3a)

`hotspot` target: `load`, then the hotspot pass and the top 100 functions. `coupling` target:
`calls`, then the call graph and the coupling pass. Default config (every corpus is loaded as
one first-party group, so the stdlib and the venv are judged like project code). 14-core M4 Pro,
release, warm cache, best of 3 (RSS worst of 3), load average about 7.5 from other worktrees.
Both passes are sequential.

| corpus | functions | classes | `hotspot.scan` | `hotspot.top` | findings (PM1xx) | `hotspot_kb` | peak RSS MB (`load` / `hotspot`) |
|--------|-----------|---------|----------------|---------------|------------------|--------------|----------------------------------|
| stdlib | 20,875    | 3,471   | 0.40ms         | 0.06ms        | 2,494            | 34           | 27.8 / 27.3                      |
| venv   | 744,507   | 159,758 | 14.9ms         | 1.68ms        | 67,095           | 1,997        | 489.6 / 492.6                    |

| corpus | modules | counted import edges | god-class candidates | `graph.build` | `coupling.analyse` | findings (PM2xx) | `coupling_kb` | `callgraph_kb` | peak RSS MB (`calls` / `coupling`) |
|--------|---------|----------------------|----------------------|---------------|--------------------|------------------|---------------|----------------|------------------------------------|
| stdlib | 1,043   | 3,958                | 200                  | 0.73ms        | 0.25ms             | 95               | 20            | 522            | 29.1 / 29.3                        |
| venv   | 60,529  | 310,372              | 3,463                | 29.2ms        | 7.97ms             | 2,828            | 1,210         | 21,808         | 584.3 / 607.6                      |

- **Hotspots**: 20 ns per function on the venv (17M LOC): a scan over the def rows and their
  metrics rows, two logarithms per function for the maintainability index, findings pushed
  with typed arguments (no formatting). `top_functions` is a bounded heap, O(defs log k).
  Memory is the 33 B per file of `ModuleHotspots` (2 MB on the venv) plus the findings; the
  peak RSS differences against `load` are within run-to-run noise.
- **Coupling**: 8 ms on the venv: one pass over the module edges (310k counted, type-only
  ones dropped), one over the defs and bases for abstractness, then the call-graph fan-out
  of the 3,463 classes above `max-class-wmc` only. `ModuleCoupling` is 20 B per file (1.2 MB).
  The 23 MB of RSS over `calls` is the call graph (`callgraph_kb`), which a full run builds
  once and shares with the other graph analyses.

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
- **Path lookup**: a segment's path index is keyed by `hash_bytes`, which does collide on real
  paths (`msgraph/.../im_csch/im_csch_request_builder.py` and `.../im_sech/...`). Colliding
  paths are kept in a side list and compared by bytes; before S4.3d they were not found, so
  those files were extracted again and their (first-party) segment rewritten on every load.

### Duplication (S4.3d)

`dupes` target vs `load` (same session): fingerprints are taken in the loader's extraction
pass, then `analysis::dupes` matches them with the default thresholds (`dupes-min-tokens = 50`,
`dupes-min-lines = 6`). `dupes-cold` / `dupes-warm` go through the facts cache, which stores the
fingerprints with the facts. Every corpus file counts as first-party (one group, one cache
segment), so the venv row is a stress test (17M LOC, much of it vendored or generated
code). 14-core M4 Pro, release, warm OS file cache, best of 3 (RSS worst of 3);
load average about 7-8 from other worktrees, so times are upper bounds. Measured after
merging S2.5 (cache).

| corpus | target       | threads | load.extract | cache.write | seeds  | relex  | extend | locate | group  | total   | peak RSS MB |
|--------|--------------|---------|--------------|-------------|--------|--------|--------|--------|--------|---------|-------------|
| stdlib | `load`       | 14      | 32.2ms       | -           | -      | -      | -      | -      | -      | 38.8ms  | 26.5        |
| stdlib | `dupes`      | 14      | 35.1ms       | -           | 2.23ms | 20.4ms | 0.55ms | 10.2ms | 0.17ms | 78.3ms  | 40.1        |
| stdlib | `dupes-cold` | 14      | 35.7ms       | 4.29ms      | 2.26ms | 20.5ms | 0.51ms | 12.2ms | 0.18ms | 83.7ms  | 41.6        |
| stdlib | `dupes-warm` | 14      | 3.69ms       | -           | 2.29ms | 23.0ms | 0.49ms | 11.1ms | 0.18ms | 47.8ms  | 41.4        |
| stdlib | `load`       | 1       | 245.3ms      | -           | -      | -      | -      | -      | -      | 298.1ms | 19.7        |
| stdlib | `dupes`      | 1       | 291.7ms      | -           | 7.53ms | 143ms  | 2.18ms | 135ms  | 0.19ms | 604.7ms | 29.5        |
| venv   | `load`       | 14      | 2.07s        | -           | -      | -      | -      | -      | -      | 3.05s   | 493.8       |
| venv   | `dupes`      | 14      | 2.15s        | -           | 95.5ms | 1.60s  | 46.7ms | 1.26s  | 48.3ms | 6.25s   | 870.0       |
| venv   | `dupes-cold` | 14      | 2.15s        | 164ms       | 96.9ms | 1.58s  | 46.3ms | 1.25s  | 48.2ms | 6.37s   | 929.9       |
| venv   | `dupes-warm` | 14      | 126ms        | -           | 94.3ms | 1.55s  | 48.8ms | 1.30s  | 47.9ms | 4.12s   | 879.6       |

Counters: stdlib 137,776 fingerprints (1.65 MB), 19,570 seeds (12,359 extended, 7,210
covered, 1 unverified), 1.63M tokens cached while extending, 1,290 file re-reads (26.5 MB),
1,649 pairs, 2,056 copies in 777 groups. Venv 4.71M fingerprints (56.5 MB), 2.52M seeds (792
unverified), 68.3M tokens cached, 85,769 re-reads (1.22 GB), 373k pairs, 228k copies in 43.5k
groups. `findings` = `copies` on every corpus: members without a partner outside themselves
are dropped. Members are bounded by their anchor copy (see `cluster`), so copies of one file
may overlap and `duplicated_lines` (54,410 on stdlib, 10.7M on the venv) counts such lines
more than once. Warm and cold find exactly what the uncached load finds (same counters).

- **Off**: zero cost (the loader's extraction loop is monomorphised without the visitor).
- **Extraction** (on): +2.9 ms on stdlib with 14 threads, within noise on the venv. 12 B per
  fingerprint retained, about one fingerprint per 16 normalised tokens (3.2 LOC on stdlib):
  3.8 B/LOC, 3.3 B/LOC on the venv.
- **Cache**: fingerprints add 8 B each to the first-party segment (stdlib 1.1 MB of 8.0 MB,
  venv 37.7 MB of 320 MB). A warm load copies them with the facts: the load part of `dupes`
  drops from 35 ms to 3.7 ms on stdlib and from 2.15 s to 126 ms on the venv (total 6.25 s to
  4.12 s). Writing the segment costs 4.3 ms / 164 ms on a cold run. A first-party segment
  without fingerprints of the requested key (written by a run with `analysis.dupes` off, or
  other thresholds) is extracted and rewritten once, i.e. costs one cold load. Peak RSS of a
  warm run includes the mapped segment of the group being copied (`load_peak_rss_mb` 836 vs
  551 MB uncached on the venv, whose single group maps all 320 MB at once); the run's peak is
  in matching either way.
- **Matching time** is dominated by the two re-reads (`relex`, `locate`), which are I/O-bound
  like `read`; seeding, extension and grouping together are 7 us/kLOC (stdlib) to 11 us/kLOC
  (venv). stdlib: +40 ms over `load`; venv: +3.2 s. Recording line numbers while re-lexing
  would skip `locate` (1.3 s on the venv) but needs about 4 more bytes per cached token (start
  line delta, lines spanned, start column) for every seeded file: +270 MB on the venv's 68M
  cached tokens, on top of the 870 MB peak, to save a re-read of the 30-50% of files that hold
  candidates. Not done: the re-read is cheap with a warm OS cache and memory is the scarcer
  resource at this size.
- **Matching memory**: the peak is in extension, `load_peak_rss_mb` + 2 B per cached token
  (normalised code + exact hash) + 16 B per seed. Seeding transiently holds 12 + 20 B per
  fingerprint. Venv: load peak 551 MB, 870 MB at the end (+319 MB). stdlib: 31 -> 40 MB.
- **Seeds that fail verification** (`unverified`) are hash collisions, files changed since
  extraction, or files where the parser's token stream diverges from the lexer's after a
  syntax error (every later position shifts, so their clones are missed): 1 on stdlib, 792 on
  the venv.

Design choices measured on stdlib (counters `copies`, `duplicated_lines`, `seeds`):

| seeding of large buckets (> 8 occurrences of a hash)             | copies | lines  | seeds       |
|------------------------------------------------------------------|--------|--------|-------------|
| every pair (reference, quadratic)                                 | 1959   | 57,486 | 199,865,146 |
| star from the first occurrence (salvaged WIP)                     | 1832   | 53,432 | 64,187      |
| ... plus runs of close hits in one file seed once                 | 1829   | 53,200 | 17,842      |
| paths by next-fingerprint context                                  | 1851   | 53,574 | 17,670      |
| paths by next and previous context                                 | 1857   | 53,765 | 19,570      |
| ... plus splitting differing data out of matches                   | 1945   | 51,165 | 19,570      |
| ... plus members bounded by their anchor copy (shipped)            | 2056   | 54,410 | 19,570      |

Every-pair seeding takes 3.1 s and 3.3 GB on stdlib. With splitting on in both (and the earlier
clustering), it finds 2022 copies to our 1945; the lines only it covers are almost all data tables (`encodings/*`,
`idna/uts46data.py`) and runs of like entries shorter than `min_tokens` (`pickletools` opcode
table, `Decimal` context methods). Without the run compression, a
synthetic pair of 30k-token periodic tables took 1.5 s (quadratic: one diagonal per hit);
with it 9.5 ms. Splitting found the codec classes of the `encodings` modules, which were
previously lost because each match ran on into the differing mapping tables and was then
rejected as data (it reports fewer lines because those tables are no longer counted).

### Cycles and layers (S4.3b)

`cycles` target, 14-core M4 Pro, release, warm cache, best of 3 (RSS worst of 3), other
worktrees running. Both analyses are O(V + E); the cycle search is sequential (graph build,
Tarjan, one BFS per cyclic component), the contract check is parallel per chunk of 256 files.

| corpus | modules | cycle edges | cycles | in cycles | largest | longest witness | `cycles` | `layers` | `layers.transitive` | rows checked | findings (direct / transitive) | `layers_kb` / transitive | peak RSS MB |
|--------|---------|-------------|--------|-----------|---------|-----------------|----------|----------|---------------------|--------------|--------------------------------|--------------------------|-------------|
| stdlib | 1,046   | 2,617       | 9      | 257       | 223     | 3               | 0.15ms   | 0.14ms   | 0.33ms              | 4,480        | 471 / 1,504                    | 41 / 105                 | 28.3        |
| venv   | 70,732  | 310,295     | 492    | 11,907    | 3,415   | 10              | 14.2ms   | 5.26ms   | 23.7ms              | 839,828      | 49,840 / 53,194                | 2,746 / 8,353            | 533.3       |

- **Time**: on the venv (17M LOC) the cycle analysis is 14 ms and the four contracts 5 ms
  (24 ms transitive, of which most is building the import graph), against 3.3 s for the whole
  pipeline. `bench-forbidden-all` makes every importable file a source, so all 840k rows of
  importable `.py` files are checked: the worst case per row (stubs, shadowed and
  non-importable files are skipped; on the stdlib that leaves 4,480 of 10,040 rows, the rest
  are in the base interpreter's `site-packages` and similar). The cycle graph has fewer edges
  than `graph`'s `import_edges` (329,342): stubs, shadowed files and type-only edges are left
  out (492 cycles instead of 520).
- **Memory**: the cycle result keeps 4 bytes per module in a cycle plus the witnesses (80 KB
  on the venv); the graph (`8 (n + 1) + 10 m` bytes) and the BFS scratch (4 bytes per module)
  are dropped before returning. Contracts cost 8 bytes per module for the module space plus 4
  per module and role array; transitive contracts add the import graph (shared by contracts
  with the same `allow-type-checking`) and 4 bytes per module and container (8 for
  independence). Peak RSS is that of the index and the import resolution.
- Witnesses are short even in large components (3,415 modules, witness of at most 10
  modules): the BFS finds a shortest cycle through the component's lowest module.

### Dead code (S4.3c)

`deadcode` target (after `calls`), 14-core M4 Pro, release, warm cache, best of 3 (RSS worst of
3), other worktrees running. Every corpus is one first-party group, so the stdlib and the venv
are judged like project code. Application mode (the default of the target) has the fewest roots
(`<module>` scopes, tests, `__main__` guards) and so the most candidates for the name fallback;
library mode adds the public API.

| corpus / mode | roots | live | candidates | probes | `deadcode.roots` | `.graph` | `.propagate` | `.evidence` | `.findings` | sum | findings (>= 10 / >= 80) | `graph_kb` | peak RSS MB (`calls` / `deadcode`) |
|---------------|-------|------|------------|--------|------------------|----------|--------------|-------------|-------------|-----|--------------------------|------------|------------------------------------|
| stdlib, application | 1,043   | 5,384   | 12,864  | 75,145    | 0.12ms | 1.21ms | 1.03ms | 2.85ms  | 0.08ms | 5.3ms  | 6,710 / 192        | 749    | 28.7 / 31.1  |
| stdlib, library     | 11,389  | 15,872  | 3,557   | 75,145    | 0.17ms | 1.24ms | 1.11ms | 1.01ms  | 0.05ms | 3.6ms  | 2,736 / 27         | 749    | 28.7 / 29.9  |
| venv, application   | 167,516 | 287,259 | 555,209 | 3,898,127 | 7.99ms | 52.6ms | 56.4ms | 129.0ms | 4.30ms | 250ms  | 306,983 / 70,727   | 30,829 | 584.6 / 685.9 |
| venv, library       | 683,698 | 816,480 | 197,658 | 3,898,127 | 11.8ms | 55.2ms | 63.8ms | 50.4ms  | 2.74ms | 184ms  | 147,356 / 51,662   | 30,829 | 584.6 / 638.3 |

- **Time**: 250 ms for the 17M-LOC venv in application mode against 3.2 s for the pipeline up
  to resolved calls. `deadcode.graph` is the call graph without override edges plus the
  override graph and the seed pass; `deadcode.propagate` the bucket-queue propagation over defs
  (1.16M expansions for 1M nodes: each node is expanded about once per distinct level it
  takes) plus the module import and module call graphs and their propagation;
  `deadcode.evidence` the candidate tables, the explanation pass (parallel per chunk of files,
  plus an SCC over the call edges between candidates), the probe of 3.9M evidence hashes
  (parallel per file) and the second propagation. The first version of the evidence scan
  probed every candidate of a name for every file mentioning it: 17.3 s on the venv (common
  method names are shared by thousands of candidates and mentioned by thousands of files);
  grouping candidates by name and bounding each probe by the smaller side of its intersection
  brought it to 0.54 s sequential, 0.11-0.13 s in parallel.
- **Memory**: the level and why bytes (2 per node, `deadcode_kb` with the module levels and
  roots) are what the result keeps. The transient peak on the venv is about 100 MB above
  `calls`: the call graph (31 MB, which a full run could share with the other graph
  analyses), the module graphs, the evidence tables (a 4-byte candidate index per node, about
  40 bytes per candidate, the candidate SCC) and the 307k findings.
- **Findings**: the venv is not an application, so application mode reports most of its
  public API; library mode is the meaningful setting there. On the stdlib in library mode, the
  27 findings at the default gate were checked by hand: most are code called only from C
  (`_strptime._strptime_time`, `sqlite3.dump._iterdump`, `_pylong`), public names of private
  modules that nothing imports (confidence 80), and genuinely unused private methods. See the
  module docs of `analysis::deadcode` for the known blind spots (calls from C, dispatch by
  computed names in other modules, dead recursive functions).

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

### Check (S4.5)

The whole `pymapper check` (config, roots, walk, cached load, import resolution, every analysis
with a default rule, filter, sort, output to a file), measured with `pymapper check --stats`
(phases on stderr) under `/usr/bin/time -l`. 14-core Apple M4 Pro, release build (fat LTO),
warm OS file cache, nothing else running. "Cold" = cache directory removed before the run
(every file read, parsed and written to the cache); "warm" = every file a cache hit, best of 3.
Default rules (PM1xx-PM4xx; `analysis.dupes` off unless noted), default confidence gate.

Corpora:

- `stdlib`: CPython 3.13.5 `lib/python3.13` checked as a first-party project (630 files,
  305k LOC, 11 MB; this install has no `test` package and its `site-packages` is excluded).
- `venv`: the `aiml-lectio` venv's site-packages (60,529 files, 17.0M LOC, 650 MB) checked as
  first-party code through a config with `src` = `paths` = the site-packages directory,
  `exclude = ["__pycache__"]`, `respect-gitignore = false`: every analysis runs over 17M LOC.
- `lectio`: the `aiml-lectio` project itself (1,803 first-party files, 478k LOC), alone and with
  `--deps --stdlib` (63,017 files, 17.8M LOC: third-party code from `RECORD` lists, the stdlib
  walked; analyses report first-party code only).

| corpus | flags | run | total | walk | load.extract | resolve.calls | deadcode | findings | peak RSS |
|--------|-------|-----|-------|------|--------------|---------------|----------|----------|----------|
| stdlib | | cold | 54-66 ms | 12 ms | 31 ms | 1.3 ms | 5.8 ms | 2,035 | 22-23 MB |
| stdlib | | warm | 19.1 ms | 1.5 ms | 3.1 ms | 1.1 ms | 6.2 ms | 2,035 | 25-28 MB |
| stdlib | `-j 1` | cold | 208 ms | | 166 ms | | 5.6 ms | 2,035 | 18.6 MB |
| stdlib | `-j 1` | warm | 23.3 ms | | 4.5 ms | | 5.8 ms | 2,035 | 18.1 MB |
| stdlib | dupes | cold | 77.6 ms | | 28.9 ms | | | 3,332 | 29.9 MB |
| stdlib | dupes | warm | 48.4 ms | | 4.0 ms | | | 3,332 | 31-32 MB |
| venv | | cold | 3.42 s | 764 ms | 1.95 s | 51 ms | 243 ms | 141,138 | 696 MB |
| venv | | warm | 1.50 s | 863 ms | 116 ms | 46 ms | 244 ms | 141,138 | 742-752 MB |
| venv | dupes | cold | 6.84 s | | 2.15 s | | | 353,298 | 975 MB |
| venv | dupes | warm | 4.65 s | | 123 ms | | | 353,298 | 0.99-1.04 GB |
| lectio | | cold | 161 ms | | | | | 1,489 | 42.5 MB |
| lectio | | warm | 102 ms | 40 ms | 4.6 ms | 1.3 ms | 3.6 ms | 1,489 | 45-49 MB |
| lectio | `--deps --stdlib` | cold | 2.75 s | 39 ms | 2.16 s | 48 ms | 91 ms | 1,510 | 685 MB |
| lectio | `--deps --stdlib` | warm | 516 ms | 44 ms | 91 ms | 50 ms | 89 ms | 1,510 | 679-700 MB |
| lectio | `--deps --stdlib -j 4` | cold | 3.55 s | | 2.89 s | | | 1,510 | 659 MB |
| lectio | `--deps --stdlib -j 4` | warm | 656 ms | | 113 ms | | | 1,510 | 640-643 MB |
| lectio | `--deps --stdlib -j 1` | cold | 10.4 s | | 8.97 s | | | 1,510 | 632 MB |
| lectio | `--deps --stdlib -j 1` | warm | 1.36 s | | 260 ms | | | 1,510 | 602-605 MB |

Wall clock (`real`) is within 10-40 ms of `total`, except on the first run of a freshly built
binary, which pays about 0.4-0.6 s of macOS executable verification before `main` (seen as
`real` 0.47-0.68 s against a 61-66 ms `total` on the stdlib); the table excludes it.
dupes phases: `dupes` = matching (seeds, re-lex, extend, locate, group); fingerprinting is
inside `load.extract`. Cache sizes after a cold run: stdlib 4.6 MB (5.3 MB with fingerprints),
venv 269 MB (305 MB with fingerprints), lectio `--deps --stdlib` 284 MB in 493 segments.

Observations:

- **Where warm time goes.** On the 17M-LOC venv as first-party code, the walk is 58% of a warm
  run (0.86 s; the walker alone measures 0.78 s on this tree, see "Walker (S2.2)") and dead code
  16%; the cached load is 0.19 s (60k `stat`s plus copying facts out of one 280 MB segment).
  Real projects walk only first-party code: with `--deps --stdlib` the same 17.8M LOC load
  warm in 0.52 s, of which the walk is 44 ms.
- **Roots discovery** costs 42-68 ms on lectio whether or not `--deps` is on (every
  `*.dist-info` of the venv is parsed for attribution): 40% of the 102 ms warm first-party-only
  run. Follow-up for the roots step: cache the distribution table per site-packages, or parse
  `RECORD` lazily when third-party analysis is off.
- **Analyses** are small next to loading: on 17M LOC hotspot 14 ms, cycles 13 ms, coupling
  7 ms, call resolution 45 ms, dead code 244 ms (of which name evidence 130 ms), output of
  141k findings 33 ms.
- **Memory.** Peak RSS is the index (facts, metrics, interner) plus the analysis tables; warm
  runs peak 40-60 MB higher than cold ones on the venv because the touched pages of the mapped
  cache segment count as resident while facts are copied out. One thread saves only 10-15%
  of peak memory against 14 threads.
- **Thread scaling** (lectio `--deps --stdlib`): cold 10.4 s / 3.55 s / 2.75 s at 1 / 4 / 14
  threads (extraction-bound: 2.9x at 4 threads, 3.8x at 14, flattening like the walker and the
  cache benchmarks); warm 1.36 s / 0.66 s / 0.52 s.
- **dupes** is the only gated analysis: +20-30 ms on 305k LOC, +3.1 s matching and +0.2 s of
  fingerprinting on 17M LOC (peak 0.70 -> 1.0 GB), which is why it is off by default.

### Query API (S5.1)

`query` target: `calls`, then `pymapper_core::query` (see the target table). 14-core M4 Pro,
release, warm cache, best of 3 (RSS worst of 3), other worktrees busy. Per-query latencies
are means over the batch with one reused `Scratch`. The worst case is the slowest single query
of the best run. Only the fuzzy lookup uses more than one thread.

Query state, built once (lazy tables forced in their own phases):

| corpus | nodes   | call edges | query.build | query.names | query.imports | query.cycle_graph | `query_kb` (of which names) | `scratch_kb` | peak RSS MB (`calls` / `query`) |
|--------|---------|------------|-------------|-------------|---------------|-------------------|-----------------------------|--------------|---------------------------------|
| stdlib | 25,441  | 31,873     | 0.71ms      | 0.13ms      | 0.12ms        | 0.13ms            | 734 (101)                   | 416          | 28.1 / 30.3                     |
| venv   | 998,345 | 1,382,184  | 33.8ms      | 5.99ms      | 12.1ms        | 13.9ms            | 35,181 (3,993)              | 16,055       | 595.3 / 646.9                   |

Per query (mean / worst; mean result size in brackets):

| query                         | stdlib (437k LOC)       | venv (17M LOC)            |
|-------------------------------|-------------------------|---------------------------|
| lookup exact qid              | 0.78us / 6us            | 10.6us / 149us            |
| lookup bare name (suffix)     | 1.9us / 23us (6)        | 60us / 1.0ms (9)          |
| lookup two segments (suffix)  | 0.63us / 3us (1)        | 10.5us / 141us (2)        |
| lookup fuzzy (one edit)       | 130us / 293us (10)      | 4.7ms / 13.7ms (11)       |
| callers, depth 1              | 0.06us / 7us (1)        | 0.62us / 432us (1)        |
| callees, depth 3              | 0.13us / 3us (5)        | 2.1us / 63us (350)        |
| callers, transitive           | 0.48us / 66us (36)      | 6.1us / 480us (752)       |
| impact, transitive            | 3.2us / 166us (36)      | 93us / 2.1ms (752)        |
| path, reachable pair          | 0.47us / 8us (5 nodes)  | 4.5us / 294us (5 nodes)   |
| path, random pair (no path)   | 0.12us / 4us            | 0.94us / 58us             |
| deps out, module level        | 0.80us / 24us (6)       | 3.0us / 77us (10)         |
| deps in, module level         | 1.3us / 14us (4)        | 3.1us / 94us (4)          |
| import cycles of a module     | 0.51us / 5us            | 2.3us / 103us             |
| call cycle through a def      | 0.28us / 14us           | 4.8us / 172us (96)        |
| summary                       | 0.96ms                  | 46.3ms                    |

- **Time**: every query except summary and fuzzy lookup answers in microseconds on 17M LOC.
  The cost tracks what the query visits, not the program size: a bounded walk reads a few CSR
  rows. A bidirectional path search grows the smaller frontier and stops at the first meet.
  `deps` scans only the target's files plus the files of its callers or callees. Building the
  query state costs 34 ms (two passes over the 2.2M call edges into a labelled CSR). The lazy
  tables add 6 ms (name index: an LSD radix sort of node ids by name), 12 ms (import graph) and
  14 ms (cycle import graph plus Tarjan).
- **Lookup**: an exact or dotted qid is one hash lookup per module prefix plus a child scan
  per def segment. A bare name is a binary search in the name index plus a ranking of all its
  candidates (`__init__` alone has over 20k, hence the 1 ms worst case). Fuzzy matching only
  runs when nothing else matched. It scans the 403k distinct venv names (8 MB) in parallel
  chunks. A byte-set prefilter drops most names after one pass over their bytes, and the edit
  distance is banded (O(len x budget)) and stops as soon as a whole row exceeds the budget.
  Sequential and unfiltered, it took 67 ms per query (201 ms worst).
- **impact** is the transitive callers plus a per-module count (sort and run-length), a
  test-file check per distinct file, and a breadth-first walk of the reverse import graph for
  the transitive importers. The importer walk is most of the gap to plain callers (6 us).
- **summary** is O(V + E): one pass over the nodes and files, the resolution counters already
  kept, the cycle SCCs, and bounded heaps for the rankings, including two `top_functions` passes
  over every def row.
- **Memory**: the query state is 35 MB on the venv: the labelled call graph (forward and
  reverse CSR, `8 (V + 1) + 10 E` bytes) and a 4 MB name index (4 B per node), plus the module
  graphs. A scratch is 8 B per node per side and the long-lived queries use both sides (16 MB).
  Its zeroed pages are only touched where queries go. Peak RSS is 52 MB above `calls`, which is
  the query state plus the scratch. Results hold node ids and small `Copy` rows, never strings.
- Correctness: `query::tests` compares walks, paths, deps (three filters, modules and classes)
  and call cycles against plain BFS, Tarjan and hash-map brute force. It runs them on the test
  corpus plus a generated project with dense cross-module calls, cycles and cross-file
  overrides.

### Deps (S6.1)

`pymapper deps [--score] --stats` on `lectio` (1,803 first-party files) with its venv (491
distributions, 62,331 files, 17.5M LOC in total; the stdlib off), cache in a temporary
directory. 14-core Apple M4 Pro, release build, warm OS file cache. "Cold" = cache directory
removed (every file extracted, every scorecard computed); "warm facts, cold scores" =
`scores.pmc` deleted, facts cached; "warm" = everything cached. Phases from `--stats`; peak RSS
is `ru_maxrss`. Warm rows: 2-3 runs, range shown.

| run | threads | total | resolve.calls | deps.usage | deps.score | scorecards | peak RSS |
|-----|---------|-------|---------------|------------|------------|------------|----------|
| `--score`, cold | 14 | 6.96 s (load.extract 5.74 s) | 59.9 ms | 20.4 ms | 4.5 ms | 491 computed | 634 MB |
| `--score`, warm facts, cold scores | 14 | 428 ms | | 19.9 ms | 5.4 ms | 491 computed | 655 MB |
| `--score`, warm facts, cold scores | 1 | 1.34 s | 471 ms | 20.4 ms | 18.8 ms | 491 computed | 566 MB |
| `--score`, warm | 14 | 503-748 ms | 52-79 ms | 20.4-22.0 ms | 0.1 ms (+0.1-0.9 ms read) | 490 cached, 1 computed | 628-656 MB |
| no `--score`, warm | 14 | 575-593 ms | | 20.0-20.1 ms | - | - | 625-635 MB |
| `check --deps`, warm (reference) | 14 | 732-787 ms | 54-60 ms | - | - | - | 659-686 MB |

The cold `load.extract` (5.7 s) was I/O-bound in that session; "Check (S4.5)" measured 2.16 s
for the same files. `scores.pmc` holds 491 scorecards in 47 KB. The one scorecard computed on
every warm run is a distribution without a `RECORD` hash (no cache key).

Observations:

- **Scorecards are cheap**: one pass over the definitions and module edges of 491
  distributions costs 18.8 ms on one thread and 5.4 ms on 14 (parallel per distribution). The
  cache turns that into a 0.1 ms lookup plus reading a 47 KB file.
- **Usage** (one pass over the first-party imports and all call edges, a forward CSR of the
  call and import graphs, a BFS from the first-party nodes that reached 109k of 1.02M nodes,
  then summing exclusive lines per distribution) is 20 ms, sequential. Its transient memory is,
  by construction, 4 bytes per node and per edge for the forward CSR, 4 bytes per node of line
  weights, two bits per node (BFS and used-symbol sets) and the BFS queue (4 bytes per reached
  node); it is freed before the report and stays below the load's peak, so peak RSS is that of
  loading the venv (as `check --deps`).
- **Whole-run cost** is the venv load plus call resolution, as for `check --deps`: `deps` is
  15-25% faster than `check --deps` warm because it runs no rule analyses.

### Test-code evidence (S4.6)

`deadcode` target, 14-core M4 Pro, release, warm, best of 3-5 (RSS worst), before (rewrite
`394d5b6`) and after. The evidence scan now reads every first-party `.py` file (not only the
importable ones) and keeps test-code occurrences in a second slot per name group.

| corpus | candidates | findings (>= 10) | `deadcode.evidence` | peak RSS MB |
|--------|------------|------------------|---------------------|-------------|
| stdlib, before | 12,883 | 6,877 | 4.55ms | 32.0 |
| stdlib, after  | 18,004 | 10,141 | 6.68ms | 35.7 |
| venv, before   | 572,011 | 313,846 | 277.1ms | 751.9 |
| venv, after    | 572,124 | 313,917 | 273.1ms | 754.7 |

- The stdlib corpus contains `site-packages/pip` (411 files, not importable below the stdlib
  root, so not analysed before): those files account for the extra candidates and findings.
  In `check`, `site-packages` is third-party and never analysed.
- A first version filtered each group's explained-file events by slot while scanning them and
  took 311 ms on the venv; sorting the events by (group, slot, file) so each slot scans only
  its own run, and skipping the test slot once production code links a def, brought it back
  to the baseline.
- `check --select PM4` on aiml-lectio (1,803 first-party files, 741 of them not importable,
  warm): dead code 5.2-5.7 ms -> 6.7-7.4 ms (evidence 2.2-2.5 -> 3.1-3.4 ms) for 40% more
  analysed files; peak RSS +1.5 MB.

### Coverage evidence (S6.3)

`deadcode` target with `XTASK_DEADCODE_COVERAGE=synthetic`, 14-core M4 Pro, release, warm, best
of 3 (RSS worst), other worktrees benchmarking at the same time. The target generates a
coverage.py JSON report for every file of the corpus (absolute paths; every line executed
except the bodies of every third function, which are missing; lines stand in for statements,
so the report is larger than a real one), maps it with `Coverage::parse` (`coverage.parse`),
drops the report bytes (as `check` does) and runs the analysis with it.

| corpus | report MB | files matched | `coverage.parse` | executed / not executed nodes | levels lowered | `deadcode.*` sum (without -> with) | peak RSS MB (without -> with) |
|--------|-----------|---------------|------------------|-------------------------------|----------------|------------------------------------|-------------------------------|
| stdlib | 2.4  | 1,043 / 1,043   | 1.82ms | 15,024 / 7,072    | 2,679  | 10.4ms -> 5.4ms | 34.1 -> 33.4 |
| venv   | 97.8 | 52,122 / 60,529 | 86.2ms | 471,496 / 213,103 | 64,696 | 482ms -> 289ms  | 752.1 -> 751.4 |

- **Time**: about 1.1-1.3 GB/s of report: one pass of the pull reader (no tree, reused
  buffers), one hash lookup per file path, a binary search per def and line list. The 8,407
  venv entries that match no analysed file (stubs, shadowed files) go through the suffix
  fallback (one sorted basename table, built once) and stay unmatched. The analysis itself got
  faster here because executed code is live: fewer name-fallback candidates (572k -> 163k on
  the venv) mean a cheaper evidence scan. The synthetic split is arbitrary, so the finding
  counts of these runs say nothing about precision.
- **Memory**: the result is two bit sets over the nodes (`coverage_kb`: 249 KB for 1M nodes).
  The report bytes are held only while they are mapped (`Coverage::load` reads, maps and drops
  them), so they add to the peak only when the report is larger than what the analysis
  allocates later; here the peak did not move.

### Incremental updates (S3.6)

`incremental` target: `Program::update` after a single-file edit, against the full load and
resolution of the `calls` target (same session). 14-core Apple M4 Pro, release (fat LTO), warm
OS file cache, 14 threads, best of 3 (RSS worst of 3); load average 4-9 from other worktrees,
so times are upper bounds. Medians are over all updates of a run (an edit and its restore count
as two updates).

| corpus | files | full load + resolve (`calls` total) | update median | best | worst | hot modules | files re-resolved (imports / calls, median) | tracking | peak RSS MB (`calls` / `incremental`) |
|--------|-------|--------------------------------------|---------------|------|-------|-------------|----------------------------------------------|----------|----------------------------------------|
| tests  | 29     | 1.67 ms | 0.35 ms | 0.18 ms | 0.46 ms | -       | 1 / 1 | 0 KB    | 5.2 / 6.0     |
| stdlib | 1,043  | 47.2 ms | 2.10 ms | 1.39 ms | 3.71 ms | 2.90 ms | 1 / 6 | 34 KB   | 29.7 / 37.7   |
| venv   | 60,529 | 3.19 s  | 29.7 ms | 28.7 ms | 47.0 ms | -       | 2 / 2 | 3.4 MB  | 596.5 / 671.0 |

The venv (650 MB) is not copied: its files are re-extracted with unchanged contents, which runs
the whole pipeline (extraction, splice, hierarchy, node map, dependency check, carried
resolution, diff) but produces empty diffs. The stdlib and tests rows edit for real (a function
appended to a file, then removed).

Phase medians per update (µs):

| corpus | plan | extract | patch (splice + hierarchy + node map) | resolve imports | resolve calls | diff |
|--------|------|---------|----------------------------------------|-----------------|---------------|------|
| stdlib | 41   | 330     | 222                                    | 424             | 1,060         | 2    |
| venv   | 3,336 | 185    | 10,067                                 | 2,193           | 13,883        | 39   |

- **What an update costs.** Reading and extracting, the bulk of a load, is paid for the changed
  file only. The splice of the fact columns is a memmove of the rows behind the file. The
  hierarchy is copied in runs with only the changed file's def block rebuilt (no sort, no
  hashing of other files). Per-file resolution runs for the files that read the changed file
  (median 1-6 on the stdlib, 2 on the venv), and every other file's results are translated in
  place, in parallel. What remains is linear in the program with small constants: the old ->
  new node map (4 bytes per node, part of the diff), the class table and C3 MROs and the method
  index (rebuilt: on the venv `resolve.classes` + `resolve.mro` are about 11 ms of the 30 ms),
  and the plan (path comparison of 60k files).
- **Speedup**: 22x on the stdlib, 107x on the venv against a full load and resolution.
- **Dependency tracking** costs 4 bytes per (file, file read) pair: 384k import and 280k call
  pairs on the venv (3.4 MB, 0.7% of the program), 1.8k and 3.7k on the stdlib. The tracked
  resolution of the initial load costs 5-10% more than the plain one on the venv
  (`resolve.imports` 7.1 vs 6.5 ms, `resolve.calls` 49.4 vs 46.1 ms; the `resolve.*` columns
  of the two targets).
- **Memory.** `Program::heap_bytes` stays flat across updates (venv 464.3 -> 464.2 MB; stdlib
  10.67 -> 10.90 MB: spliced columns grow with 1/8 headroom, not by doubling). Peak RSS is 12%
  above a one-shot load on the venv: an update rebuilds the class table, MROs and method index
  next to the old ones, copies the node table once and holds the node map. Before results were
  carried in place (old and new resolution tables alive together) the venv peaked at 760 MB,
  and before exact reservations of the edge arrays peak RSS crept up by 40 MB per update
  (1.6 GB after 36 updates).
- **When the whole program is resolved again** (still re-extracting only what changed): files
  added, removed, renamed or reordered, a module name or status changed (a new stub, a new
  `__init__.py`), the interner compacted, or a name the resolvers look up by text (`object`,
  `__init__`, a builtin) interned for the first time. Then an update costs the extraction of
  the changed files plus a full tracked resolution (the `resolve.*` phases of `calls`).
- **Follow-ups**: patch the class table, MROs and method index per file (the largest remaining
  cost on 17M LOC); a fixed-size node map representation for the diff when only offsets move.
