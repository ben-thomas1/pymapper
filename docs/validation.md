# Validation

Checks performed on 30 September 2026 on Apple Silicon macOS, using a separate
checkout containing only the files intended for publication:

- Rust 1.93 debug and release: **226 tests passed in each configuration**, plus
  all 14 Criterion benchmark smoke cases through `cargo test --all-targets`.
- Rust nightly AddressSanitizer: all 226 tests passed with the standard library
  and native C components instrumented. Upstream LLVM Clang and an explicit
  macOS SDK were used because Apple Clang's sanitizer ABI differed from Rust's.
- Actual debug and release TUI: graph navigation, Unicode labels, source files
  shortened after analysis, tiny-terminal resizing, Ctrl-C, invalid input exits,
  and terminal mode/alternate-screen restoration passed on a Unix PTY.
- Both binaries built and installed into a temporary Cargo installation prefix.
  Eleven documented CLI examples ran successfully. v1 and v2 DOT output rendered
  through Graphviz, including a path containing a quote.
- A generated 300-function chain produced the expected 299 calls, reachability
  of 300 and depth of 299. Analysis did not execute or modify the input source.
  Saving and comparing a baseline also passed.
- `cargo fmt --all --check`, documentation tests and patch whitespace checks
  passed. Clippy completed with nonblocking style/structure suggestions, mainly
  collapsible conditionals, argument counts and large enum variants.

Regression tests reproduce failures in input/error handling, duplicate inputs,
relative and scoped imports, nested resolution, parameter shadowing, pytest
entrypoint detection, complexity/maintainability calculations, confidence
filtering, graph depth, baseline overwrite protection, report sorting and
threshold validation. The test corpus is synthetic.

The GitHub Actions workflow defines Linux and macOS checks but has not yet run
on a hosted runner. Linux, Windows and WebAssembly runtime behavior were not
tested in this session. No performance or accuracy calibration against real
projects is claimed. Clippy is not configured as a warning-free gate.
