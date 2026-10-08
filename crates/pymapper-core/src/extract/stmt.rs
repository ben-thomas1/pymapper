//! Statements, blocks and definitions.

use super::events::Visitor;
use super::parser::Parser;
use crate::lex::TokenKind as K;

impl<V: Visitor> Parser<'_, V> {
    /// Parses the whole file.
    pub fn parse_file(&mut self) {
        loop {
            match self.kind(0) {
                K::EndOfFile => return,
                K::Newline | K::Indent | K::Dedent => {
                    self.bump();
                }
                _ => self.skip_line(),
            }
        }
    }
}
