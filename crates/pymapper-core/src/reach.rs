//! Reachability via SCC condensation + HyperLogLog sketches (design: Performance rules).
//! Owned by the reach step.
//!
//! ```text
//!   Index (+ Imports, ResolvedCalls, EntrypointMatcher, script targets)
//!     └─> roots()        RootSet: one root_kind byte per node (module scopes, __main__,
//!                        tests, entrypoint decorators / names / bases, scripts, public API)
//!   call Graph + RootSet
//!     └─> forward()      Reached: BFS bitset + shortest-path predecessor per node, O(V + E)
//!   Condensed (SCCs + DAG) + Weights (symbol bit, own lines per node)
//!     └─> reach_sizes()  ReachSizes per node: symbols reached, lines reached ("LOC reached")
//!                        Approx(p): HLL sketches merged sinks first (default)
//!                        Exact:     DFS per component, gated (`analysis.exact-reachability`)
//! ```
//!
//! # Roots
//!
//! [`roots`] collects what is live without being called (design: dead code (a), (c)-(e)):
//! `<module>` pseudo-scopes (module-level code runs on import), `__main__` guards and
//! `__main__` modules, tests, declarative entrypoints ([`EntrypointMatcher`](crate::config::EntrypointMatcher): decorator, base
//! class and name globs, presets included), `[project.scripts]` / `entry_points.txt` targets,
//! and in library mode the public API ([`is_library`] resolves `mode = "auto"`). Only files of
//! the selected origins contribute (first-party by default). See [`RootOptions`].
//!
//! # Forward reachability
//!
//! [`forward`] is a breadth-first search over the forward CSR of any [`Graph`](crate::graph::Graph) over node ids
//! (normally [`call_graph`](crate::graph::call_graph), which includes override edges so dynamic
//! dispatch keeps overrides alive). One bit per node marks it live, one `u32` per node records
//! the BFS predecessor ([`Reached::path_to`]: why a node is live). O(V + E).
//!
//! # Reach sizes
//!
//! For every node `v`, [`reach_sizes`] gives `|R(v)|` restricted to symbols (classes and
//! functions) and the lines of `R(v)`, where `R(v)` is every node reachable from `v`, itself
//! included. Lines are exclusive per node ([`Weights::of_index`]), so the lines of a set are
//! the distinct source lines it covers. Members of one SCC reach the same set, so sizes are
//! computed per component of the [`Condensed`](crate::graph::Condensed) DAG and shared.
//!
//! **Approximate (default, `SizeMode::Approx(p)`)**: components are processed in reverse
//! topological order (sinks first, [`Condensed::reverse_topo`](crate::graph::Condensed::reverse_topo)); a component's set is the union
//! of its members' elements (one key per symbol, one key per own line) and its successors'
//! sets. Sets are exact sorted key arrays while they hold at most `2^p / 4` keys (4 bytes each,
//! never more bytes than a dense sketch) and HyperLogLog sketches of `2^p` one-byte registers
//! beyond ([`hll`]); unions are merges, so an element reached along several paths counts once.
//! A component's sets are freed once its last predecessor has merged them (reference count =
//! DAG in-degree), and that last consumer takes over a dense sketch's registers instead of
//! copying them; freed register arrays are pooled.
//!
//! - Time: O(V + E_dag + total lines) for keys and sparse unions (plus sorting small key
//!   arrays), and O(2^p) per merge into or estimate of a dense sketch: O(E_dag 2^p) worst case.
//! - Memory: the live sets: at most one pair per component that has unprocessed
//!   predecessors, `2^p` bytes per dense sketch, 4 bytes per key of a sparse one
//!   ([`SizeStats`] reports the peaks). Output: 8 bytes per node.
//!
//! **Error bounds**: a size of at most `2^p / 4` is exact (up to 32-bit key collisions,
//! probability `n^2 / 2^33` of one among `n` elements). A larger size is a HyperLogLog
//! estimate (Ertl's improved estimator, unbiased): relative standard error `1.04 / sqrt(2^p)`,
//! 3.25% at the default `p = 10` (1.63% at `p = 12`, 6.5% at `p = 8`), so about 95% of
//! estimates are within twice that and almost all within three times; key collisions add a
//! bias of `-n / 2^33` (-0.2% at 16M lines). Estimates are deterministic (fixed hash, fixed
//! merge order) and the same set always gives the same estimate.
//!
//! **Exact (`SizeMode::Exact`)**: a depth-first search over the DAG from every component, in
//! parallel chunks with a visit stamp per component and chunk. Cost O(sum over components of
//! the DAG part they reach), worst case O(k (k + E)): quadratic when most components reach a
//! large common part (call graphs with a big hub SCC). Memory O(k) per chunk (`4 k` bytes) on
//! top of the output. Gated behind `analysis.exact-reachability`; measured costs are in
//! `docs/benchmarks.md` ("Reachability (S3.5)").

pub mod hll;

mod bits;
mod exact;
mod forward;
mod roots;
mod sizes;

#[cfg(test)]
mod tests;

pub use bits::{BitSet, Ones};
pub use forward::{Reached, forward};
pub use roots::{RootOptions, RootSet, is_library, root_kind, roots};
pub use sizes::{DEFAULT_PRECISION, ReachSizes, SizeMode, SizeStats, Weights, reach_sizes};
