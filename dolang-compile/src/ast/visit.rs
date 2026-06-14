use std::{
    fmt::{self, Debug, Formatter},
    ops::ControlFlow,
};

use crate::source::Span;

use super::origin;

pub trait Node {
    const TRANSPARENT: bool = false;

    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break>;

    fn trans<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        if Self::TRANSPARENT {
            self.accept(visit)
        } else {
            visit.node(self)
        }
    }

    fn span(&self) -> Span {
        let mut visit = SpanVisit(Span::INVALID);
        let _ = self.accept(&mut visit);
        visit.0
    }

    fn kind(&self) -> NodeKind;
}

struct SpanVisit(Span);

impl Visit for SpanVisit {
    type Break = ();

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        self.0 = self.0 | node.span();
        ControlFlow::Continue(())
    }

    fn token(
        &mut self,
        _leaf: Token,
        span: Span,
        _origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        self.0 = self.0 | span;
        ControlFlow::Continue(())
    }
}

/// Syntactic token
#[derive(Copy, Clone, Debug)]
pub enum Token {
    /// Comment
    Comment,
    /// Non-numeric, non-string constant such as `nil` or `false`
    Constant,
    /// Delimiter such as `(`, `[`, etc
    Delim,
    /// Escape within a string
    Escape,
    /// Field, e.g. `bar` in `foo.bar`
    Field,
    /// Key such as `foo:`
    Key,
    /// Module name
    ModuleName,
    /// Module item
    ModuleItem,
    /// Keyword such as `while`, `for`, `do`, `import`
    Keyword,
    /// Literal string, including non-escape, non-interpolated portions of quoted strings
    Literal,
    /// Numeric constant
    Number,
    /// Unary or binary operator such as `+` or `-`
    Operator,
    /// A string delimeter (`"`)
    StringDelim,
    /// A variable
    Variable,
    /// A sigil like `$` or `...`
    Sigil,
}

pub trait Visit {
    type Break;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break>;
    fn token(
        &mut self,
        token: Token,
        span: Span,
        origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break>;
}

/// Classification of AST nodes
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Literal,
    Ident,
    I64,
    VerbatimI64,
    F64,
    VerbatimF64,
    Bool,
    Nil,
    Sym,
    Concat,
    Escape,
    BinConcat,
    EscapeByte,
    Group,
    Unary,
    Binary,
    Call,
    Lambda,
    Field,
    Index,
    Array,
    Dict,
    Error,
    Assign,
    Bind,
    Break,
    Class,
    Continue,
    Decorator,
    Def,
    For,
    If,
    Import,
    Let,
    Return,
    Throw,
    Try,
    While,
    NlGuard,
    Param,
    Block,
    Function,
    ImportItem,
    Branch,
    Catch,
    Expand,
    Pair,
    Body,
    Pattern,
    Key,
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}
