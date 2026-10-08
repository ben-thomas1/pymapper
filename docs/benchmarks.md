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
| `lex`        | `walk`, `read+lex`     | same, lexing each file to EOF (parser mode, no trivia)    |
| `lex-mem`    | `walk`, `load`, `lex`  | lexer micro benchmark: load the corpus into memory, then lex it from memory (no I/O in `lex`) |

The parse hook is `parse_file(src: &[u8], acc: &mut ParseAcc)` in `xtask/src/bench/targets.rs`.
Later steps plug the lexer/extractor in there. `ParseAcc` is per-thread state that gets merged
at the end; its `counters()` (e.g. tokens, defs) show up in the table and the results file.
Until then the hook does nothing, so `parse-only` costs the same as `read`. The walk is an
interim sequential walker in `xtask/src/bench/walk.rs`. It should be replaced by
`pymapper_core::walk` once that exists.

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

Lexer (S1.1), same machine, release build, warm cache, best of 5. Other worktrees were
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
