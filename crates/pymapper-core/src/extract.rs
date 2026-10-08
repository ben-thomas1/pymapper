//! Parser, FileFacts and metrics (design: Front end, Metric definitions).
//!
//! The parser ([`parser`]) is a recursive descent over statements and blocks
//! with a token-scanning expression level. It reports what it sees through the
//! [`events::Visitor`] trait; facts and metrics are independent visitors.

mod compound;
mod defs;
pub mod events;
mod expr;
pub mod parser;
mod soft;
mod stmt;

#[cfg(test)]
mod tests;

pub use events::{NoopVisitor, Visitor};
pub use parser::{Parser, parse};
