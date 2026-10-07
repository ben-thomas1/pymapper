# pymapper

Whole-program analysis for Python codebases: import cycles, layering contracts,
cross-module dead code, duplication, coupling and dependency hygiene, with
ruff-style output, rule codes, config and exit codes.

**Status:** under rewrite. The CLI subcommands exist but are not implemented yet.
See [`docs/design.md`](docs/design.md) for the design.

## Layout

```
crates/pymapper-core   library (all analysis)
crates/pymapper-cli    the `pymapper` binary
xtask                  dev tooling: cargo xtask bench | diff-ruff | fuzz
tests/corpus           Python inputs used by tests and benchmarks
docs/design.md         design document (source of truth)
```

## Build

```
cargo build --release          # target/release/pymapper
cargo test --workspace
cargo build --profile profiling   # release + line tables, for sampling profilers
```

The toolchain is pinned in `rust-toolchain.toml`.

## Licence

MIT, see [`LICENSE`](LICENSE).
