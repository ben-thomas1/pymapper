//! Shared plumbing of the query subcommands (`callers`, `callees`, `impact`, `path`, `imports`,
//! `cycles`, `summary`): whole-program loading, symbol resolution and output.
//!
//! ```text
//!   ConfigArgs ──> config ──> pipeline::load (as `check`) ──> resolve_calls ──> Query
//!   SYMBOL ──> Query::resolve ──┬─ unique ────> the subcommand's query ──> rows
//!                               ├─ ambiguous ─> candidates (stdout), error (stderr), exit 2
//!                               └─ not found ─> similar names (stdout), error (stderr), exit 2
//!   rows ──> positional PATHS filter (whole program loaded either way) ──> concise | json
//! ```
//!
//! Output conventions (both formats): paths relative to the project root, 1-based lines and
//! byte columns, symbols as stable qualified ids (`pkg.mod:Class.method`), call edges tagged
//! with their confidence (`certain`, `likely`, `possible`) and `dispatch` when only an
//! override edge (base method -> override) links them. Concise output is one fact per line
//! (`path:line:col: fact [confidence]`) followed by one summary line; JSON follows `check`'s
//! conventions (version 2, `tool`, one result object per line, `summary`).
//!
//! Exit codes: 0 answered (empty results included), 1 `path` found no path, 2 usage or fatal
//! error, ambiguous or unknown symbol.

pub mod hops;
mod render;
mod session;
mod sites;

pub use render::{Writer, finish, origin_str};
pub use sites::{Pair, import_site, locate};
pub use session::{QueryFlags, Session, WalkFlags, parse_conf};

/// Exit code of a query that found nothing it was asked to prove (`path`: no path).
pub const EXIT_NOT_FOUND: u8 = 1;
