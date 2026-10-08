//! [`Tee`]: drive two visitors from one parse (e.g. facts + metrics).

use super::events::{
    Assign, Base, Block, BlockKind, BoolOp, BranchKind, Call, DeclKind, Decorator, Def, DefKind, Diagnostic, End,
    Import, Pos, Visitor,
};
use crate::lex::Token;

/// Forwards every event to `.0`, then to `.1`. Static dispatch: events
/// neither visitor uses still compile away.
pub struct Tee<A, B>(pub A, pub B);

impl<A: Visitor, B: Visitor> Visitor for Tee<A, B> {
    #[inline(always)]
    fn token(&mut self, tok: Token) {
        self.0.token(tok);
        self.1.token(tok);
    }
    #[inline(always)]
    fn decorator(&mut self, dec: &Decorator<'_>) {
        self.0.decorator(dec);
        self.1.decorator(dec);
    }
    #[inline(always)]
    fn base(&mut self, base: &Base<'_>) {
        self.0.base(base);
        self.1.base(base);
    }
    #[inline(always)]
    fn def_enter(&mut self, def: &Def<'_>) {
        self.0.def_enter(def);
        self.1.def_enter(def);
    }
    #[inline(always)]
    fn def_exit(&mut self, kind: DefKind, end: End) {
        self.0.def_exit(kind, end);
        self.1.def_exit(kind, end);
    }
    #[inline(always)]
    fn block_enter(&mut self, block: &Block<'_>) {
        self.0.block_enter(block);
        self.1.block_enter(block);
    }
    #[inline(always)]
    fn block_exit(&mut self, kind: BlockKind, end: End, last_case: bool) {
        self.0.block_exit(kind, end, last_case);
        self.1.block_exit(kind, end, last_case);
    }
    #[inline(always)]
    fn type_alias(&mut self, name: Token, type_params: &[Token], pos: Pos) {
        self.0.type_alias(name, type_params, pos);
        self.1.type_alias(name, type_params, pos);
    }
    #[inline(always)]
    fn branch(&mut self, kind: BranchKind, tok: Token) {
        self.0.branch(kind, tok);
        self.1.branch(kind, tok);
    }
    #[inline(always)]
    fn bool_op(&mut self, op: BoolOp, tok: Token) {
        self.0.bool_op(op, tok);
        self.1.bool_op(op, tok);
    }
    #[inline(always)]
    fn lambda_enter(&mut self, tok: Token) {
        self.0.lambda_enter(tok);
        self.1.lambda_enter(tok);
    }
    #[inline(always)]
    fn lambda_exit(&mut self, end: u32) {
        self.0.lambda_exit(end);
        self.1.lambda_exit(end);
    }
    #[inline(always)]
    fn call(&mut self, call: &Call<'_>) {
        self.0.call(call);
        self.1.call(call);
    }
    #[inline(always)]
    fn keyword_arg(&mut self, name: Token) {
        self.0.keyword_arg(name);
        self.1.keyword_arg(name);
    }
    #[inline(always)]
    fn import(&mut self, import: &Import<'_>) {
        self.0.import(import);
        self.1.import(import);
    }
    #[inline(always)]
    fn assign(&mut self, assign: &Assign<'_>) {
        self.0.assign(assign);
        self.1.assign(assign);
    }
    #[inline(always)]
    fn decl(&mut self, kind: DeclKind, names: &[Token]) {
        self.0.decl(kind, names);
        self.1.decl(kind, names);
    }
    #[inline(always)]
    fn name(&mut self, tok: Token) {
        self.0.name(tok);
        self.1.name(tok);
    }
    #[inline(always)]
    fn attribute(&mut self, tok: Token) {
        self.0.attribute(tok);
        self.1.attribute(tok);
    }
    #[inline(always)]
    fn string(&mut self, tok: Token) {
        self.0.string(tok);
        self.1.string(tok);
    }
    #[inline(always)]
    fn diagnostic(&mut self, diag: &Diagnostic) {
        self.0.diagnostic(diag);
        self.1.diagnostic(diag);
    }
}
