# Fuzzing: `cargo xtask fuzz`

A stable-Rust mutation fuzzer for the lexer and parser. It needs no nightly toolchain, no
cargo-fuzz and no extra crates. A target is a plain `fn(&[u8])` that must never panic, hang or
kill the process, whatever bytes it gets.

```
cargo xtask fuzz                        # default target (extract), 60 s, all cores, seeds from tests/corpus
cargo xtask fuzz -t demo -T 600         # one target for 10 minutes
cargo xtask fuzz -n 1000000 --seed 42   # exactly inputs 0..1000000 of seed 42
cargo xtask fuzz ~/src/some-project     # extra seeds: directories (*.py, *.pyi) or files
cargo xtask fuzz --list                 # registered targets
cargo xtask fuzz -t demo --reproduce target/fuzz-artifacts/demo/crash-....py
cargo xtask fuzz -t demo --seed 42 --replay 1234 > input.py   # regenerate input 1234
```

Exit codes: 0 = nothing found, 1 = crashes, hangs or process deaths found, 2 = usage or fatal
error.

## How it works

```
cargo xtask fuzz  (supervisor, debug build)
 |  builds xtask with --profile fuzz (optimised, panic=unwind, debug assertions and
 |  overflow checks on) unless --use-current-exe
 +-> xtask fuzz --child  (fuzz-profile build)
 |     worker threads x N  -- generate input k, run target(input) under catch_unwind
 |     watchdog (main)     -- per-input time budget, abandons stuck threads
 |     in-flight file      -- mmap'd: which iteration each worker is running
 +-> after a child dies: replay its in-flight inputs one per process
     (xtask fuzz --reproduce -), minimise the culprit, restart from where it stopped
```

- **Inputs are numbered.** Input `k` is a pure function of `(seed, k, seed files, --max-len)`.
  Every input has its own xorshift64* stream seeded through splitmix64. So any input can be
  regenerated from its number (`--replay`), runs are repeatable (`--seed`, printed at start),
  and `-n N` runs exactly inputs `start..start+N` whatever the thread count. Workers claim
  iterations from a shared counter in batches of 16. Nothing is copied per input: each worker
  reuses one buffer.
- **Seeds**: 8 tiny built-in snippets, `tests/corpus` (unless `--no-corpus`), and the paths given
  on the command line. They are kept in one byte buffer (each truncated to `--max-len`), and
  directory files are sorted by path so seed numbering does not depend on the filesystem.
- **Mutations** (1 to 8 per input, weighted): bit flips, set an interesting byte, insert an
  interesting token (sometimes repeated up to 512 times for deep nesting or long runs), replace
  a range with a token, truncate, delete a range, duplicate a range, splice a chunk of another
  seed file, crossover (prefix of this input + suffix of another), indentation scrambles over a
  block of lines (dedent, extra spaces, tabs, mixed tabs and spaces, partial dedent, form
  feeds), and random bytes. Tokens cover quotes and every string prefix (including t-strings
  and triple quotes), brackets, backslash continuations, `\r\n` and `\r`, tabs and form feeds,
  f-string braces, conversions and nested format specs, non-ASCII, BOM, U+2028, invalid UTF-8,
  NUL, comments and `# pymapper: ignore[...]`, keywords and soft keywords, operators, and
  numeric edge forms.
- **Crashes**: panics are caught with `catch_unwind`. A panic hook records the location and
  message silently on fuzz threads. Crashes are deduplicated by panic location. The first input
  per location is saved and then minimised in-process.
- **Hangs**: a worker publishes the iteration it is running. The watchdog declares an input hung
  once it has run longer than `--budget-ms`. A compare-and-swap decides the race with the worker
  finishing. The watchdog then regenerates the input from its number, abandons the stuck thread
  and starts a replacement. After `--max-hangs` hangs the run stops, since every abandoned
  thread may keep a core busy. Hangs are minimised in fresh processes (at most 500 runs or
  60 s each). Exiting the process frees the stuck thread.
- **Process deaths** (stack overflow, `abort`, OOM kill) cannot be caught in-process. The child
  publishes its in-flight iterations in a `MAP_SHARED` file in the temp directory. When the
  child dies, the supervisor reads the file, replays each in-flight input in a fresh process to
  find the one that kills it, minimises that input the same way (at most 500 runs), and
  restarts the child from the next unclaimed iteration (at most `--max-restarts` times).
  Inputs that were claimed but not yet run when the child died are skipped.
- **Minimiser**: greedy ddmin-style chunk removal. It removes whole lines first, in chunks
  halving from half the input down to one line, then bytes the same way. It repeats until a
  round removes nothing or `--minimise-execs` runs are used up.

## Artifacts

Saved under `target/fuzz-artifacts/<target>/` (or `--out DIR/<target>/`):

| file                    | content                                                         |
|-------------------------|-----------------------------------------------------------------|
| `<kind>-<hash>.raw.py`  | the input as generated, written as soon as the finding is known |
| `<kind>-<hash>.py`      | the minimised input                                             |
| `<kind>-<hash>.txt`     | kind, target, seed, iteration, panic location and message, and the `--reproduce` / `--replay` commands |

`<kind>` is `crash`, `hang` or `abort`. For crashes, `<hash>` is a hash of the panic location,
so re-finding a known bug overwrites its files rather than piling up copies. For hangs and
aborts it is a hash of the input. Findings whose minimised input equals one already saved in the
same run are dropped.

## Options

| option                 | default        | meaning                                                  |
|------------------------|----------------|----------------------------------------------------------|
| `-t, --target NAME`    | first target   | target to fuzz (`--list`)                                |
| `-s, --seed N`         | from the clock | RNG seed (printed)                                       |
| `-n, --iterations N`   | unlimited      | inputs to run                                            |
| `-T, --time SECS`      | 60 (unless -n) | wall-clock limit                                         |
| `-j, --jobs N`         | all cores      | worker threads                                           |
| `--budget-ms MS`       | 1000           | per-input time budget; slower inputs count as hangs      |
| `--max-len BYTES`      | 65536          | maximum input length                                     |
| `--max-hangs N`        | 8              | stop after this many hangs                               |
| `--minimise-execs N`   | 4000           | runs per minimisation (0 = save raw inputs only)         |
| `--stack-mb MB`        | 8              | worker thread stack size                                 |
| `--max-restarts N`     | 3              | restarts after the fuzzing process dies                  |
| `--no-corpus`          | off            | do not seed from tests/corpus                            |
| `--out DIR`            | target/fuzz-artifacts | artifact directory                                |
| `--profile NAME`       | fuzz           | cargo profile for the fuzzing process (must unwind)      |
| `--use-current-exe`    | off            | fuzz with the running binary (tests; debug is slow)      |

## Targets

The registry is `TARGETS` in `xtask/src/fuzz/targets.rs`. The first entry is the default. Add
a target by appending a `FuzzTarget { name, about, selftest, run }`, where `run: fn(&[u8])`.
Use internal assertions (`debug_assert!` stays on in the fuzz profile) to check invariants such
as tokens tiling the input or spans staying in bounds.

| target           | what it does                                                             |
|------------------|--------------------------------------------------------------------------|
| `extract`        | facts + metrics in one pass (`extract_with` + `MetricsVisitor`): one `<module>` row, parents before children and enclosing them, def spans/lines in bounds and ordered, decorator/base rows grouped by owner with counts matching each def's, every scope and fact line inside the file, sorted unique evidence, one metrics row per def with sane values; rendering resolves every name and chain (bounds); a second extraction into the same store with an interner is identical and renders the same |
| `lex`            | lexer in both modes: tokens in order, in bounds, at most `3 * len + 8` of them, sticky EOF; trivia mode minus comments/NL equals parser mode, same suppressions; a checkpoint taken mid-line and restored replays the rest of the stream exactly |
| `parse`          | parser events: def/block/lambda events balance, consumed tokens in order and in bounds |
| `demo`           | toy tokenizer with invariant checks (tests the fuzzer itself)            |
| `selftest-panic` | planted panics on NUL and `{{{{`, used to test the fuzzer                |
| `selftest-hang`  | planted 1.5 s stall on a UTF-8 BOM                                       |
| `selftest-abort` | planted unbounded recursion (stack overflow) on a backtick               |

## Throughput

Measured on the 14-core Mac, `--profile fuzz`, all cores, demo target:

| seeds                          | seed bytes | execs/s     | peak RSS (fuzzing process) |
|--------------------------------|-----------:|------------:|---------------------------:|
| tests/corpus + built-ins       | 8.5 kB     | ~15 million | 4.1 MB                     |
| + CPython 3.13 stdlib          | 13.8 MB    | ~380 k      | 20.8 MB                    |

These numbers mostly measure input generation, because the demo target is trivial. Expect
real targets to be limited by the target itself.

## Campaigns (S1.5)

Front end at the end of S1.5, 14 threads, default budget (1 s per input) and `--max-len`
(64 KiB), 14-core M4 Pro shared with other work:

| target    | seeds                         | time   | execs      | execs/s | crashes | hangs |
|-----------|-------------------------------|--------|------------|---------|---------|-------|
| `extract` | tests/corpus + stdlib 3.13    | 600 s  | 31,656,959 | 52.8 k  | 0       | 0     |
| `lex`     | tests/corpus + stdlib 3.13    | 300 s  | 55,230,010 | 184 k   | 0       | 0     |
| `extract` | tests/corpus only             | 240 s  | 238,823,568 | 995 k | 0     | 0     |

The earlier, interrupted S1.5 attempt ran the same targets and found one bug (end-of-input
diagnostics placed on a line after the last one; fixed, see `docs/parity.md`).
