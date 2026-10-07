//! pymapper core library: whole-program analysis of Python codebases.
//!
//! See `docs/design.md` at the repository root for the design (source of truth).

pub mod analysis;
pub mod arena;
pub mod cache;
pub mod config;
pub mod deps;
pub mod extract;
pub mod graph;
pub mod incremental;
pub mod index;
pub mod intern;
pub mod lex;
pub mod output;
pub mod query;
pub mod reach;
pub mod resolve;
pub mod roots;
pub mod rollup;
pub mod rules;
pub mod walk;
