# pymapper rewrite - design (source of truth)

## Goals
1. One CLI ("pymapper") that agents and humans use to find problems in Python codebases, run in workflows like pytest/ruff/ty: ruff-style output, rule codes, config, suppressions, baselines, exit codes.
2. A native GUI ("pymapper-gui", macOS Metal + Linux Wayland/Vulkan, NO web) to SEE packages/modules/symbols, the links between them, and quality metrics, with semantic zoom.
3. ULTRA performance on compute time AND memory is a first-class requirement. Target: millions of LOC. Features that are too slow get gated behind config/flags with documented cost.
4. Optional (toggle) analysis of the venv (third-party) and stdlib, linking first-party code to them and scoring them (less prominently than first-party).
5. Focus on whole-program analyses ruff/ty cannot do (they are per-file): import cycles, layering contracts, cross-module dead code, duplication, coupling/instability, dependency hygiene; complexity as metrics.

## Measured baseline (old code, 14-core Mac)
- tree-sitter: 143 MB/s on 14 cores (22 MB/s/core). ruff parser: 1.8 GB/s (217 MB/s/core). Rough hand tokenizer: 8.4 GB/s (842 MB/s/core).
- Venv corpus: 52k .py files, 620 MB, 16M LOC. Sequential walk 3.2s (2.7 GB dir), parallel read 1.5s warm.
- Old v2 scan of stdlib (437k LOC): 13.3s single-threaded, dominated by malloc/format in linear-scan resolution.

## Crates
- crates/pymapper-core  : library. Modules: lex, extract (parser + facts + metrics), intern, arena, walk (+ gitignore), roots, index, cache, resolve, graph, rollup, reach, incremental, watch (own FSEvents / inotify watcher), config, rules, analysis/{hotspot,coupling,cycles,layers,deadcode,dupes}, output, query, deps.
- crates/pymapper-cli   : "pymapper" binary.
- crates/pymapper-gui   : "pymapper-gui" binary (created in P7). The CLI must not depend on GPU crates.
- xtask                 : dev tooling only (bench, diff-ruff, fuzz). Never shipped.
- tests/corpus          : Python inputs (old fixtures + sample_python).

## Dependency policy (write our own by default)
OWN: lexer, parser, interner, hasher (Fx-style), arena, walker, gitignore matcher, cache format (no serde), graph algorithms, HLL sketches, layout, file watching.
ALLOWED: rayon, clap (CLI only), toml (config parsing only), thiserror (core lib errors), anyhow (binaries/xtask), libc (mmap / getrusage / FSEvents+inotify FFI), GUI: winit, wgpu, egui, egui-wgpu, egui-winit, glyphon, pollster. xtask may use ruff_python_parser/ruff_python_ast (git dep), serde_json, and dev conveniences. Anything else needs a very strong written justification in the PR summary and the reviewer must approve it explicitly.

## Toolchain
Pinned STABLE Rust only (rust-toolchain.toml). No nightly, no #![feature]. SIMD via std::arch with scalar fallback and only where profiling justifies it.

## Performance rules (enforced in review)
- No per-symbol String. Strings are interned u32 ids. Struct-of-arrays tables. Files referenced by FileId + byte span.
- Sources are NEVER retained after extraction (streaming read -> extract -> drop). GUI source view re-reads spans.
- Per-thread bump arenas, lock-free merge by concatenation + id remap.
- No quadratic passes in defaults. Every analysis declares a cost class (O(n), O(V+E), expensive) and expensive ones are gated.
- Reachability via SCC condensation + HyperLogLog sketches in reverse topological order; exact mode behind a flag.
- Avoid allocation in hot loops; no format! in hot paths; reuse buffers.
- Every step that touches a hot path reports time + peak RSS via xtask bench (once it exists) on tests/corpus, stdlib (/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13) and the venv (/Users/bt386671/Code/gsk/aiml-lectio/.venv/lib/python3.12/site-packages).

## Front end
- Pull lexer over &[u8] (never requires UTF-8; non-UTF-8 bytes in identifiers allowed, no NFKC). Token = {kind: u8, start: u32, len: u32} (12 bytes). INDENT/DEDENT stack, implicit bracket joining, backslash continuation, BOM, all string prefixes incl. t-strings (3.14), triple quotes, PEP 701 f/t-string nesting via inline mode stack, all numeric forms, longest-match operators. Records "# pymapper: ignore[CODE,...]" comments and ruff-style "# noqa[: CODES]" (only F401 is honoured, as PM405). Cheap checkpoint/restore (pos, bracket depth, fstring mode stack; indent stack is invariant within a logical line).
- Parser: fixed 4-slot ring buffer (power of two, idx & 3) for peek(k). Unbounded lookahead (soft keyword match) via lexer checkpoint + rescan to ':' before NEWLINE. Recursive descent over statements/blocks; expression level is a token scanner with bracket/context stack and postfix-chain recognition (a.b.c( , self.x( , super().m( , call-on-subscript/call-result = unknown receiver). Error recovery: resync at next statement, record diagnostic, never abort a run. Python 3.14 grammar; Python 2 unsupported (diagnostic). .pyi files read (preferred over .py for symbol shapes). .ipynb unsupported.
- FileFacts (SoA, per-thread arena): defs (kind, name, parent, line/byte span, decorators, bases, params, flags async/property/static/classmethod/overload/private, metrics), imports (scope, dotted module, name, alias, level, line, flags star/TYPE_CHECKING/try-ImportError/conditional), calls (scope, name chain slice, receiver kind, line/col), bindings (x = Foo(...) hints, module-level names, __all__ contents, global/nonlocal), evidence hashes (attribute names, identifier-like string literals), <module> pseudo-scope, __main__ guard flag, suppressions, diagnostics.

## Metric definitions
- McCabe: EXACTLY ruff C901 / PyCQA mccabe: statement-only. +1 per if, elif, for, while, except handler, try-else, nested def (nested def body counted into parent), +1 per match case minus 1 if last case is irrefutable and unguarded. NOT counted: boolean ops, ternaries, comprehensions, with, assert. Function base is 1. Each nested function also gets its own score.
- Cognitive: SonarSource spec. Nesting-weighted (+1 + nesting): if, ternary, match, for, while, except. Flat +1: elif, else. Boolean operator sequences: +1 per run of like operators. +1 for direct recursion. Nesting increments inside nested functions/lambdas and their complexity counts toward the parent. try/finally/with add nothing.
- Nesting depth, LOC (inclusive span), Halstead (operators/operands from tokens), MI = clamp((171 - 5.2 ln V - 0.23 CC - 16.2 ln LOC) * 100/171, 0, 100).

## Loading / index
- Own parallel walker + gitignore matcher; no symlink following; diagnostics instead of aborts.
- Import roots: pyproject, src layout, namespace packages, venv (pyvenv.cfg, site-packages, .pth incl. editable installs), stdlib per interpreter version, dist-info -> distribution (RECORD, top_level.txt). Origin tag: first-party / third-party / stdlib. Toggles for deps and stdlib.
- Index: stable qualified IDs (pkg.mod:Class.method) - NOT byte offsets. Pre-order hierarchy distribution -> package -> module -> class -> function with subtree ranges (O(1) ancestor-at-depth and is-descendant).
- Cache: own flat mmap zero-copy format, versioned header, atomic writes. First-party per-file (path/size/mtime then content hash). Third-party per distribution (name+version+RECORD hash; no walk needed). Stdlib per version.

## Semantics
- Import resolution: absolute/relative, re-exports via __init__, __all__, star imports, TYPE_CHECKING (type-only edges), unresolved third-party attributed to distribution.
- Call resolution (parallel per file, read-only index): lexical scopes, builtins table, self/cls/super(), proper C3 MRO, instantiation -> __init__, local construction hints, unique-method-name fallback (possible), override liveness. Each edge: confidence (certain/likely/possible/unresolved) + reason code enum.
- Graph: CSR forward+reverse, iterative Tarjan SCC, condensed DAG, module-level import and call graphs.
- Rollups at any hierarchy level: call-site count, distinct pairs, import flag, worst confidence. Bottom-up aggregates: LOC, sum/max CC, finding counts, score.
- Incremental API: changed files -> re-extract -> patch index -> re-resolve dependants via reverse import graph -> index diff.

## Checks / CLI
- Config: [tool.pymapper] in pyproject.toml or pymapper.toml; fully commented template documenting every option and its perf cost (cost numbers TBD from benchmarks; leave clear placeholders). Framework presets: flask, fastapi, django, click, pytest, pydantic.
- Rules: codes PMxxx, severity, confidence 0-100, cost class, select/ignore, thresholds, gating.
- Dead code: (a) builtins, <module> caller, __main__ guard, __all__/re-exports, instantiation -> __init__, dunder/protocol methods; (b) override liveness; (c) reachability from roots instead of fan-in; (d) declarative entrypoint rules (decorator globs, base classes, [project.scripts], entry_points.txt) + presets; (e) library vs application mode; (f) name-reference fallback via evidence hashes; (g) per-finding confidence score; (h, P6) coverage json evidence; (i) suppressions/baseline/whitelist. check gates on confidence >= threshold (default ~80).
- Output: concise "path:line:col: CODE message", json, sarif, github. Exit 0 clean / 1 findings / 2 usage or fatal error. Graceful on closed stdout (EPIPE). Baselines by stable id.
- Queries: callers, callees, impact, path, deps, cycles, summary - compact, agent-friendly output. Same core query API used by the GUI.
- Third-party: per-distribution scorecards (pymapper deps --score), never affect check exit code; hygiene: unused declared deps, undeclared imports, private-internals imports.

## GUI
- winit + wgpu own instanced renderer for the canvas (rects, rounded rects, lines/curves as instanced quads, glyph atlas text, camera transform in shader, spatial grid culling/hit test, label LOD), egui only for panels. Render on demand (zero idle CPU). Layout on background thread.
- Primary "code map": deterministic squarified treemap sized by LOC (stable geography), hierarchical edge bundling, semantic zoom switching rollup level, selectable node size/colour/edge-weight metrics (call sites, distinct pairs, LOC reached, API surface used, complexity-weighted). Third-party/stdlib in a muted region.
- Interaction: hover neighbourhood, select/focus, search-jump, path A->B, overlays (cycles, layer violations, findings, confidence filter), source pane. Secondary: force-directed (Barnes-Hut, own quadtree), dependency matrix, N-hop focus. Watch mode via own FSEvents/inotify layer + incremental API.
