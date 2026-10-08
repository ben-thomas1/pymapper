//! Compound statements: if/while/for/try/with/match.

use super::events::Visitor;
use super::expr::S_COLON;
use super::parser::Parser;

impl<V: Visitor> Parser<'_, V> {
    /// Placeholder: header up to `:`, then the suite.
    pub(super) fn compound(&mut self) {
        self.bump();
        self.expr(S_COLON);
        self.header_colon();
        self.suite();
    }
}
