//! Type syntax.
//!
//! Types are annotations only: elaboration and lowering ignore them.

use std::ops::ControlFlow;

use super::{
    Expr, Ident,
    visit::{Node, NodeKind, Token, Visit},
};
use crate::{doc, source::Span};

/// A type expression
pub(crate) enum TypeExpr {
    /// A possibly dotted name, e.g. `Str` or `time.Duration`
    Name {
        head: Ident,
        fields: Vec<Span>,
        /// What the head names when that is not a variable. Set only when documenting.
        decl: Option<TypeDecl>,
    },
    /// A constant: a symbol, string, integer, boolean or `nil`
    Const { expr: Box<Expr> },
    /// Type arguments applied to a type, e.g. `Array[Int]`
    App {
        base: Box<TypeExpr>,
        args: Vec<TypeArg>,
        bracket_span: Span,
    },
    /// A schema, e.g. `{name: Str, ?port: Int}`
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

/// A binder or type-only import named by a type, neither of which has a variable
pub(crate) struct TypeDecl {
    /// The declared name, which identifies the declaration
    pub(crate) span: Span,
    /// The declaration's document node
    pub(crate) node: Option<doc::Id>,
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
    /// `...`, for an unrestricted schema rest
    OpenRest { ellipsis_span: Span },
    /// `...K: V`, for any number of keyed items
    KeyRest {
        ellipsis_span: Span,
        key_ty: TypeExpr,
        colon_span: Span,
        ty: TypeExpr,
    },
}

pub(crate) enum TypeKey {
    /// A bareword key, which is a symbol
    Sym(Span),
    /// A key given by a type, such as a quoted string or a parenthesized name
    Type(Box<TypeExpr>),
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
    pub(crate) bound: Option<Box<Annot>>,
    pub(crate) default: Option<Box<BinderDefault>>,
    /// The trailing `,`
    pub(crate) delim_span: Option<Span>,
    /// The binder's document node, which it has no variable to carry
    pub(crate) node: Option<doc::Id>,
}

pub(crate) struct BinderDefault {
    pub(crate) equal_span: Span,
    pub(crate) ty: TypeExpr,
}

pub(crate) enum BinderKind {
    /// `T`
    Pos,
    /// `:K`
    Key { colon_span: Span },
    /// `...R`
    Rest { ellipsis_span: Span },
}

impl TypeExpr {
    /// Visit each name within the type: its head, what the head names besides a variable,
    /// and the fields dotted onto the head.
    pub(crate) fn each_name<F: FnMut(&mut Ident, &mut Option<TypeDecl>, &[Span])>(
        &mut self,
        f: &mut F,
    ) {
        match self {
            TypeExpr::Name { head, fields, decl } => f(head, decl, fields),
            TypeExpr::Const { .. } | TypeExpr::Error => {}
            TypeExpr::App { base, args, .. } => {
                base.each_name(f);
                for arg in args {
                    arg.each_name(f);
                }
            }
            TypeExpr::Schema { args, .. } => {
                for arg in args {
                    arg.each_name(f);
                }
            }
            TypeExpr::Group { ty, .. } => ty.each_name(f),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    member.each_name(f);
                }
            }
            TypeExpr::Func { params, ret, .. } => {
                for param in params {
                    param.each_name(f);
                }
                ret.each_name(f);
            }
        }
    }
}

impl TypeArg {
    fn each_name<F: FnMut(&mut Ident, &mut Option<TypeDecl>, &[Span])>(&mut self, f: &mut F) {
        for ty in self.tys_mut() {
            ty.each_name(f);
        }
    }

    /// The item's key type, if it has one, then its type.
    pub(crate) fn tys_mut(&mut self) -> impl Iterator<Item = &mut TypeExpr> {
        let (key_ty, ty) = match &mut self.kind {
            TypeArgKind::Pos(ty) | TypeArgKind::Rest { ty, .. } => (None, Some(ty)),
            TypeArgKind::Key { key, ty, .. } => (
                match key {
                    TypeKey::Sym(_) => None,
                    TypeKey::Type(key_ty) => Some(&mut **key_ty),
                },
                Some(ty),
            ),
            TypeArgKind::KeyRest { key_ty, ty, .. } => (Some(key_ty), Some(ty)),
            TypeArgKind::OpenRest { .. } => (None, None),
        };
        key_ty.into_iter().chain(ty)
    }
}

impl Node for TypeExpr {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            TypeExpr::Name { head, fields, decl } => {
                match decl {
                    Some(decl) => visit.token(Token::Type, head.span, decl.node)?,
                    None => visit.token(
                        Token::Type,
                        head.span,
                        head.res.as_ref().and_then(|res| res.node),
                    )?,
                }
                for field in fields {
                    visit.token(Token::Operator, field.before_left_char(), None)?;
                    visit.token(Token::Type, *field, None)?;
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
                    TypeKey::Sym(span) => visit.token(Token::TypeKey, *span, None)?,
                    TypeKey::Type(key_ty) => visit.node(&**key_ty)?,
                }
                visit.token(Token::Delim, *colon_span, None)?;
                visit.node(ty)?
            }
            TypeArgKind::Rest { ellipsis_span, ty } => {
                visit.token(Token::Sigil, *ellipsis_span, None)?;
                visit.node(ty)?
            }
            TypeArgKind::OpenRest { ellipsis_span } => {
                visit.token(Token::Sigil, *ellipsis_span, None)?;
            }
            TypeArgKind::KeyRest {
                ellipsis_span,
                key_ty,
                colon_span,
                ty,
            } => {
                visit.token(Token::Sigil, *ellipsis_span, None)?;
                visit.node(key_ty)?;
                visit.token(Token::Delim, *colon_span, None)?;
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
        self.with_ellipsis(None).accept(visit)
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
        visit.token(Token::Binder, self.ident.span, self.node)?;
        if let Some(bound) = &self.bound {
            visit.node(&**bound)?;
        }
        if let Some(default) = &self.default {
            visit.token(Token::Operator, default.equal_span, None)?;
            visit.node(&default.ty)?;
        }
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

impl Annot {
    pub(crate) fn with_ellipsis(&self, ellipsis: Option<Span>) -> impl Node + '_ {
        struct RestAnnot<'a>(&'a Annot, Option<Span>);
        impl Node for RestAnnot<'_> {
            fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
                visit.token(Token::Annotation, self.0.at_span, None)?;
                if let Some(span) = self.1 {
                    visit.token(Token::Sigil, span, None)?;
                }
                visit.node(&self.0.ty)
            }
            fn kind(&self) -> NodeKind {
                NodeKind::Annot
            }
        }
        RestAnnot(self, ellipsis)
    }
}
