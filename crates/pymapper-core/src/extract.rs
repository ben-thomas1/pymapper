//! Parser, FileFacts and metrics (design: Front end, Metric definitions).
//!
//! The parser ([`parser`]) is a recursive descent over statements and blocks
//! with a token-scanning expression level. It reports what it sees through the
//! [`events::Visitor`] trait; facts and metrics ([`metrics`]) are independent
//! visitors, run in the same pass with [`Tee`].

mod compound;
mod defs;
pub mod config;
pub mod events;
mod expr;
mod lines;
pub mod facts;
pub mod metrics;
pub mod parser;
mod soft;
mod stmt;
mod tee;

#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod tests;

pub use events::{NoopVisitor, Visitor};
pub use metrics::{MetricsScratch, MetricsTable, MetricsVisitor};
pub use parser::{Parser, parse, parse_with_suppressions};
pub use tee::Tee;
