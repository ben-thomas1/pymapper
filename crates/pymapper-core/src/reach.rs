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
//! and in library mode the public API ([`is_library`] resolves `mode = "auto"` for the whole
//! index, [`FileModes`] per project unit). Only files of the selected origins contribute
//! (first-party by default). See [`RootOptions`].
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
//! **Approximate (default, `SizeMode::Approx(p)`, `p = 10`)**: components are processed in
//! reverse topological order (sinks first,
//! [`Condensed::reverse_topo`](crate::graph::Condensed::reverse_topo)); a component's reach set
//! is its members plus its successors' sets.
//!
//! - Sparse: while a set has at most `2^p / 4` nodes and `4 * 2^p` lines it is a sorted array
//!   of node ids (4 bytes each) with exact totals; unions are linear merges that sum the
//!   weights of the nodes they add.
//! - Dense: beyond that, two HyperLogLog sketches ([`hll`]) of `2^p` one-byte registers, one
//!   over symbol keys (one per symbol node) and one over line keys (one per own line of every
//!   node), each followed by its register histogram, so an estimate costs O(32). Unions are
//!   register-wise maxima (vectorised in blocks of 16, histogram updated only where registers
//!   change); a sparse set meeting a dense one inserts its nodes' keys (at most `4 * 2^p`).
//!   An element reached along several paths counts once.
//! - A set is freed once its last predecessor has merged it (reference count = DAG
//!   in-degree); that last consumer takes over a dense buffer instead of copying it, and
//!   freed buffers are pooled.
//!
//! Costs: O(V + E_dag) merges of sparse sets of at most `2^p / 4` nodes, O(2^p / 16) block
//! compares per dense merge, at most `4 * 2^p` key inserts per sparse-into-dense merge, and one
//! pass over a node's own lines each time it enters a dense set. Memory: the live sets, at most
//! one per component with unprocessed predecessors, `2 (2^p + 136)` bytes per dense set and 4
//! bytes per node of a sparse one ([`SizeStats`] reports the peaks), plus 8 bytes per node of
//! output. Single-threaded. Measured (`docs/benchmarks.md`, "Reachability (S3.5)"): 240ms
//! for the 17M-LOC venv call graph (1M nodes, 1.26M DAG edges) with 3.4 MB of dense sets at
//! the peak; adversarial graphs where many sets stay live (random forward edges between
//! 200k blocks) hold up to 63k dense sets (142 MB).
//!
//! **Error bounds**: a set of at most `2^p / 4` nodes and `4 * 2^p` lines is exact. A larger
//! one is estimated (Ertl's improved estimator, unbiased over the whole range): relative
//! standard error `1.04 / sqrt(2^p)` per size, 3.25% at the default `p = 10` (6.5% at `p = 8`,
//! 1.63% at `p = 12`). Estimates are deterministic, and propagation is lossless with respect
//! to the sketch: a node's estimate is exactly that of a sketch built from its reach set
//! directly (checked on the venv). Nested reach sets share most keys, so their errors are
//! correlated: one unlucky shared subset shifts all its callers alike. Measured on the venv
//! against exact sizes (sizes above `2^p / 4`): RMSE 7.8% / 2.9% / 1.0% at `p = 8 / 10 / 12`,
//! worst 37% / 16% / 5.7% (5 standard errors, a cluster of callers of one subset). Keys are
//! 32 bits; collisions bias a size of `n` by about `-n / 2^33` (-0.2% at 16M lines).
//!
//! **Exact (`SizeMode::Exact`)**: a depth-first search over the DAG from every component, in
//! parallel chunks with a visit stamp per component and chunk. Cost O(sum over components of
//! the DAG part they reach), worst case O(k (k + E)): quadratic when most components reach a
//! large common part (long call chains, many callers of a big hub). Memory O(k) per chunk
//! (`4 k` bytes) on top of the output. Gated behind `analysis.exact-reachability`. Measured:
//! 50ms on 14 threads (324ms on one) for the venv, whose reach sets are small (at most 13.8k
//! symbols), but 2.2s for a synthetic 1M-node call graph with a giant SCC (36x the estimate)
//! and 196ms for a 30k-node chain (75x).

pub mod hll;

mod bits;
mod exact;
mod forward;
mod roots;
mod sizes;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod tests;

pub use bits::{BitSet, Ones};
pub use forward::{Reached, forward};
pub use roots::{FileModes, ModeReason, RootOptions, RootSet, is_library, plausible_entry, root_kind, roots};
pub use sizes::{DEFAULT_PRECISION, ReachSizes, SizeMode, SizeStats, Weights, reach_sizes};
