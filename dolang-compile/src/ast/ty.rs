//! Type syntax.
//!
//! Types are annotations only: elaboration and lowering ignore them.

use std::ops::ControlFlow;

use super::{
    Expr, Ident,
    visit::{Node, NodeKind, Token, Visit},
};
use crate::source::Span;

/// A type expression
pub(crate) enum TypeExpr {
    /// A possibly dotted name, e.g. `Str` or `time.Duration`
    Name { head: Ident, fields: Vec<Span> },
    /// A constant: a symbol, string, integer, boolean or `nil`
    Const { expr: Box<Expr> },
    /// Type arguments applied to a type, e.g. `Array[Int]`
    App {
        base: Box<TypeExpr>,
        args: Vec<TypeArg>,
        bracket_span: Span,
    },
    /// A dict schema, e.g. `{name: Str, ?port: Int}`
    Schema {
        args: Vec<TypeArg>,
        brace_span: Span,
    },
    /// A parenthesized type
    Group { ty: Box<TypeExpr>, paren_span: Span },
    /// A union, e.g. `Str | Path`
    Union {
        members: Vec<TypeExpr>,
        /// Every `|` in source order, including a leading one
        bars: Vec<Span>,
    },
    /// A function type, e.g. `(Int, ?Int) -> Int` or `Int -> Int`
    Func {
        params: Vec<TypeArg>,
        /// Absent when a single unparenthesized parameter precedes the `->`
        paren_span: Option<Span>,
        arrow_span: Span,
        ret: Box<TypeExpr>,
    },
    /// A type that could not be interpreted
    Error,
}

/// An item in `[]`, `()` or `{}` within a type
pub(crate) struct TypeArg {
    /// The `?` marking the position optional
    pub(crate) optional: Option<Span>,
    pub(crate) kind: TypeArgKind,
    /// The trailing `,`
    pub(crate) delim_span: Option<Span>,
}

pub(crate) enum TypeArgKind {
    /// `T`
    Pos(TypeExpr),
    /// `key: T`
    Key {
        key: TypeKey,
        colon_span: Span,
        ty: TypeExpr,
    },
    /// `...T`
    Rest { ellipsis_span: Span, ty: TypeExpr },
}

pub(crate) enum TypeKey {
    /// A bareword key, which is a symbol
    Sym(Span),
    /// A quoted key, which is a string
    Str(Box<Expr>),
}

/// A `@` annotation on a bound name
pub(crate) struct Annot {
    pub(crate) at_span: Span,
    pub(crate) ty: TypeExpr,
}

/// A `->` return type on a function
pub(crate) struct RetType {
    pub(crate) arrow_span: Span,
    pub(crate) ty: TypeExpr,
}

/// The binders in `[]` after the name of a `def` or `class`
pub(crate) struct Binders {
    pub(crate) binders: Vec<Binder>,
    pub(crate) bracket_span: Span,
}

/// A name that stands for a type argument
pub(crate) struct Binder {
    pub(crate) kind: BinderKind,
    pub(crate) ident: Ident,
    /// The trailing `,`
    pub(crate) delim_span: Option<Span>,
}

pub(crate) enum BinderKind {
    /// `T`
    Pos,
    /// `:K`
    Key { colon_span: Span },
    /// `...R`
    Rest { ellipsis_span: Span },
}

impl Node for TypeExpr {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            TypeExpr::Name { head, fields } => {
                visit.node(head)?;
                for field in fields {
                    visit.token(Token::Operator, field.before_left_char(), None)?;
                    visit.token(Token::Field, *field, None)?;
                }
                ControlFlow::Continue(())
            }
            TypeExpr::Const { expr } => visit.node(&**expr),
            TypeExpr::App {
                base,
                args,
                bracket_span,
            } => {
                visit.node(&**base)?;
                visit.token(Token::Delim, bracket_span.left_char(), None)?;
                args.accept(visit)?;
                visit.token(Token::Delim, bracket_span.right_char(), None)
            }
            TypeExpr::Schema { args, brace_span } => {
                visit.token(Token::Delim, brace_span.left_char(), None)?;
                args.accept(visit)?;
                visit.token(Token::Delim, brace_span.right_char(), None)
            }
            TypeExpr::Group { ty, paren_span } => {
                visit.token(Token::Delim, paren_span.left_char(), None)?;
                visit.node(&**ty)?;
                visit.token(Token::Delim, paren_span.right_char(), None)
            }
            TypeExpr::Union { members, bars } => {
                let mut bars = bars.iter();
                if bars.len() == members.len()
                    && let Some(bar) = bars.next()
                {
                    visit.token(Token::Operator, *bar, None)?;
                }
                for (index, member) in members.iter().enumerate() {
                    if index != 0
                        && let Some(bar) = bars.next()
                    {
                        visit.token(Token::Operator, *bar, None)?;
                    }
                    visit.node(member)?;
                }
                ControlFlow::Continue(())
            }
            TypeExpr::Func {
                params,
                paren_span,
                arrow_span,
                ret,
            } => {
                if let Some(paren_span) = paren_span {
                    visit.token(Token::Delim, paren_span.left_char(), None)?;
                }
                params.accept(visit)?;
                if let Some(paren_span) = paren_span {
                    visit.token(Token::Delim, paren_span.right_char(), None)?;
                }
                visit.token(Token::Operator, *arrow_span, None)?;
                visit.node(&**ret)
            }
            TypeExpr::Error => ControlFlow::Continue(()),
        }
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Type
    }
}

impl Node for TypeArg {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        if let Some(span) = self.optional {
            visit.token(Token::Operator, span, None)?;
        }
        match &self.kind {
            TypeArgKind::Pos(ty) => visit.node(ty)?,
            TypeArgKind::Key {
                key,
                colon_span,
                ty,
            } => {
                match key {
                    TypeKey::Sym(span) => visit.token(Token::Key, *span, None)?,
                    TypeKey::Str(expr) => visit.node(&**expr)?,
                }
                visit.token(Token::Delim, *colon_span, None)?;
                visit.node(ty)?
            }
            TypeArgKind::Rest { ellipsis_span, ty } => {
                visit.token(Token::Sigil, *ellipsis_span, None)?;
                visit.node(ty)?
            }
        }
        if let Some(span) = self.delim_span {
            visit.token(Token::Delim, span, None)?;
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::TypeArg
    }
}

impl Node for Annot {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Sigil, self.at_span, None)?;
        visit.node(&self.ty)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Annot
    }
}

impl Node for Binders {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Delim, self.bracket_span.left_char(), None)?;
        self.binders.accept(visit)?;
        visit.token(Token::Delim, self.bracket_span.right_char(), None)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Binders
    }
}

impl Node for Binder {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self.kind {
            BinderKind::Pos => {}
            BinderKind::Key { colon_span } => visit.token(Token::Sigil, colon_span, None)?,
            BinderKind::Rest { ellipsis_span } => visit.token(Token::Sigil, ellipsis_span, None)?,
        }
        visit.node(&self.ident)?;
        if let Some(span) = self.delim_span {
            visit.token(Token::Delim, span, None)?;
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Binder
    }
}

impl Node for RetType {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Operator, self.arrow_span, None)?;
        visit.node(&self.ty)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::RetType
    }
}
