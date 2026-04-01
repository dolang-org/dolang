#[cfg(feature = "debug")]
pub(crate) mod dot;

pub(crate) mod visit;

use std::{
    collections::VecDeque,
    fmt::Debug,
    ops::{ControlFlow, Deref},
};

use crate::{
    ast::visit::{Node, NodeKind, Token, Visit},
    origin,
    source::File,
};

use super::{lex::Op, source::Span, sym};

/// Context in which a token appears
#[derive(Copy, Clone, Debug, Default)]
pub enum Context {
    #[default]
    None,
    Call,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Var {
    // Symbol for identifier (and export key)
    pub(crate) sym: sym::Id,
    // Captured by closure?
    pub(crate) captured: bool,
    // Exported from module?
    pub(crate) exported: bool,
    // Used?
    pub(crate) used: bool,
    // Origin tracking ID
    pub(crate) origin: origin::Id,
}

impl Var {
    pub(crate) fn is_emitted(&self, origintab: &origin::Table) -> bool {
        !self.captured || !self.is_prelude(origintab) || self.used || self.exported
    }
    pub(crate) fn is_prelude(&self, origintab: &origin::Table) -> bool {
        matches!(
            origintab[self.origin],
            origin::Origin::PreludeModule { .. } | origin::Origin::PreludeItem { .. }
        )
    }
    pub(crate) fn is_synthetic(&self, origintab: &origin::Table) -> bool {
        matches!(origintab[self.origin], origin::Origin::Synthetic)
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct Res {
    pub(crate) index: usize,
    pub(crate) depth: usize,
    pub(crate) origin: origin::Id,
}

/// Information for a non-local jump (break/continue/return across closure boundary)
#[derive(Debug)]
pub(crate) struct NlInfo {
    /// Number of scope levels to traverse (converted to upvar depth during lowering)
    pub(crate) scope_depth: usize,
    /// Indicator: 1=break, 2=continue, 3=return
    pub(crate) indicator: u8,
    /// For non-local return: resolution of the synthetic upvar for the return value
    pub(crate) ret_upvar: Option<Res>,
}

pub(crate) struct Ident {
    pub(crate) span: Span,
    pub(crate) res: Option<Res>,
}

impl Ident {
    pub(crate) fn new(span: Span) -> Self {
        Self { span, res: None }
    }
}

impl Node for Ident {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(
            Token::Variable,
            self.span,
            self.res.as_ref().map(|r| r.origin),
        )
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Ident
    }
}

#[derive(Debug)]
pub(crate) struct ExprBody<T> {
    pub(crate) elems: Vec<T>,
    pub(crate) vars: Vec<Var>,
}

impl<T: Node> Node for ExprBody<T> {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        self.elems.accept(visit)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Body
    }
}

pub(crate) struct For<T> {
    pub(crate) bind: Pattern,
    pub(crate) expr: Option<Expr>,
    pub(crate) body: T,
    pub(crate) iter: Option<Res>,
    pub(crate) for_span: Span,
    pub(crate) equal_span: Option<Span>,
}

impl<T> For<T> {
    pub(crate) fn map<R>(self, f: &mut impl FnMut(T) -> R) -> For<R> {
        For {
            body: f(self.body),
            expr: self.expr,
            iter: self.iter,
            for_span: self.for_span,
            equal_span: self.equal_span,
            bind: self.bind,
        }
    }
}

impl<T: Node> Node for For<T> {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.for_span, None)?;
        visit.node(&self.bind)?;
        if let Some(expr) = &self.expr {
            if let Some(equal_span) = &self.equal_span {
                visit.token(Token::Operator, *equal_span, None)?;
            }
            visit.node(expr)?;
        }
        self.body.trans(visit)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::For
    }
}

pub(crate) struct IfBranch<T> {
    pub(crate) span: Span,
    pub(crate) cond: Expr,
    pub(crate) body: T,
}

impl<T> IfBranch<T> {
    pub(crate) fn map<R>(self, f: &mut impl FnMut(T) -> R) -> IfBranch<R> {
        IfBranch {
            body: f(self.body),
            span: self.span,
            cond: self.cond,
        }
    }
}

pub(crate) struct If<T> {
    pub(crate) tbranch: IfBranch<T>,
    pub(crate) elif_branches: Vec<(IfBranch<T>, Span)>,
    pub(crate) else_branch: Option<(T, Span)>,
}

impl<T> If<T> {
    pub(crate) fn map<R>(self, f: &mut impl FnMut(T) -> R) -> If<R> {
        If {
            tbranch: self.tbranch.map(f),
            elif_branches: self
                .elif_branches
                .into_iter()
                .map(|(v, s)| (v.map(f), s))
                .collect(),
            else_branch: self.else_branch.map(|(v, s)| (f(v), s)),
        }
    }
}

impl<T: Node> Node for IfBranch<T> {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.span, None)?;
        visit.node(&self.cond)?;
        self.body.trans(visit)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Branch
    }
}

impl<T: Node> Node for If<T> {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        // Visit first if branch
        visit.node(&self.tbranch)?;

        // Visit elif branches with their else spans
        for (elif_branch, else_span) in &self.elif_branches {
            visit.token(Token::Keyword, *else_span, None)?;
            visit.node(elif_branch)?;
        }

        // Visit final else branch if present
        if let Some((else_body, else_span)) = &self.else_branch {
            visit.token(Token::Keyword, *else_span, None)?;
            else_body.trans(visit)?
        }

        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::If
    }
}

pub(crate) struct Expand {
    pub(crate) expr: Expr,
    pub(crate) delim_span: Option<Span>,
    pub(crate) ellipsis_span: Span,
}

impl Node for Expand {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Sigil, self.ellipsis_span, None)?;
        visit.node(&self.expr)?;
        if let Some(span) = &self.delim_span {
            visit.token(Token::Delim, *span, None)?
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Expand
    }
}

pub(crate) struct Single {
    pub(crate) expr: Expr,
    pub(crate) delim_span: Option<Span>,
}

impl Node for Single {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        self.expr.accept(visit)?;
        if let Some(span) = self.delim_span {
            visit.token(Token::Delim, span, None)?
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        self.expr.kind()
    }
}

pub(crate) enum ArrayElem {
    Single(Single),
    Expand(Expand),
    For(For<ExprBody<Self>>),
    If(If<Vec<Self>>),
}

impl Node for ArrayElem {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            ArrayElem::Single(node) => node.accept(visit),
            ArrayElem::Expand(node) => node.accept(visit),
            ArrayElem::For(node) => node.accept(visit),
            ArrayElem::If(node) => node.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            ArrayElem::Single(single) => single.kind(),
            ArrayElem::Expand(expand) => expand.kind(),
            ArrayElem::For(node) => node.kind(),
            ArrayElem::If(node) => node.kind(),
        }
    }
}

pub(crate) struct Pair {
    pub(crate) key: Expr,
    pub(crate) value: Expr,
    pub(crate) colon_span: Option<Span>,
    pub(crate) delim_span: Option<Span>,
}

impl Node for Pair {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.node(&self.key)?;
        if let Some(colon_span) = self.colon_span {
            visit.token(Token::Delim, colon_span, None)?;
        }
        visit.node(&self.value)?;
        if let Some(delim_span) = self.delim_span {
            visit.token(Token::Delim, delim_span, None)?;
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Pair
    }
}

pub(crate) enum DictElem {
    Single(Single),
    Key(Key),
    Pair(Pair),
    Expand(Expand),
    For(For<ExprBody<Self>>),
    If(If<Vec<Self>>),
}

impl Node for DictElem {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            DictElem::Single(node) => node.accept(visit),
            DictElem::Key(node) => node.accept(visit),
            DictElem::Pair(node) => node.accept(visit),
            DictElem::Expand(node) => node.accept(visit),
            DictElem::For(node) => node.accept(visit),
            DictElem::If(node) => node.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            DictElem::Single(single) => single.kind(),
            DictElem::Key(pair) => pair.kind(),
            DictElem::Pair(pair) => pair.kind(),
            DictElem::Expand(expand) => expand.kind(),
            DictElem::For(node) => node.kind(),
            DictElem::If(node) => node.kind(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Const {
    Str(String),
    Bin(Vec<u8>),
    I64(i64),
    F64(f64),
    Bool(bool),
    Nil,
    Sym(Span),
    Error,
}

pub(crate) enum GroupDelim {
    Paren(Span),
    Dollar(Span),
    RawQuotes(Span, Span),
}

pub(crate) enum GetVariant {
    Normal(Span),
    SpecialMethod {
        method: SpecialMethod,
        span: Span,
        paren_span: Span,
    },
    Private {
        span: Span,
        res: Option<sym::Id>,
    },
}

pub(crate) enum Expr {
    Literal(Span),
    Ident(Ident),
    I64(i64, Span),
    VerbatimI64(i64, Span),
    F64(f64, Span),
    VerbatimF64(f64, Span),
    Bool(bool, Span),
    Nil(Span),
    Sym(Span),
    Concat {
        exprs: Vec<Expr>,
        delim_span: Option<Span>,
        arg: bool,
    },
    Escape(char, Span),
    BinConcat {
        exprs: Vec<Expr>,
        open: Span,
        close: Span,
    },
    EscapeByte(u8, Span),
    Group {
        expr: Box<Expr>,
        delim: Option<GroupDelim>,
    },
    Unary {
        op: Op,
        expr: Box<Expr>,
        op_span: Span,
    },
    Binary {
        op: Op,
        exprs: Box<[Expr; 2]>,
        op_span: Span,
    },
    Call {
        arg0: Box<Expr>,
        args: Vec<Arg>,
        delim: Option<GroupDelim>,
    },
    Lambda {
        func: Function,
        do_span: Option<Span>,
    },
    Get {
        object: Box<Expr>,
        field: GetVariant,
        dot_span: Span,
    },
    Index {
        bracket_span: Span,
        exprs: Box<[Expr; 2]>,
    },
    Array {
        bracket_span: Option<Span>,
        elems: Vec<ArrayElem>,
    },
    Dict {
        brace_span: Option<Span>,
        elems: Vec<DictElem>,
    },
    Error,
}

/// Classification of side-effectfulness for expressions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideEffect {
    /// Definitely has no side effects (pure constants)
    None,
    /// Definitely has no side effects, but references a variable
    VarRef,
    /// Usually has no side effects, but possible with overloading
    Unlikely,
    /// Likely has side effects and result is used directly
    Likely,
    /// Side effects occurred but result was processed pointlessly
    Discarded,
}
impl SideEffect {
    // Model effect of performing an operation unlikely to produce a side effect
    // on arbitrary values and certain not to on well-behaved (built-in) types
    pub(crate) fn unlikely(self) -> Self {
        match self {
            SideEffect::Discarded => SideEffect::Discarded,
            SideEffect::Likely => SideEffect::Discarded,
            SideEffect::VarRef => SideEffect::Unlikely,
            other => other,
        }
    }
}

impl Const {
    fn to_string(&self, file: &File<'_>) -> Option<String> {
        match self {
            Const::Str(v) => Some(v.clone()),
            Const::Bin(v) => Some(std::str::from_utf8(v).ok()?.to_owned()),
            Const::I64(v) => Some(v.to_string()),
            Const::F64(v) => Some(v.to_string()),
            Const::Bool(v) => Some(v.to_string()),
            Const::Nil => Some("nil".to_owned()),
            Const::Sym(span) => Some(file.str(*span).to_owned()),
            Const::Error => Some("<error>".to_owned()),
        }
    }

    fn to_bin(&self) -> Option<Vec<u8>> {
        match self {
            Const::Str(v) => Some(v.as_bytes().to_vec()),
            Const::Bin(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Expr {
    pub(crate) fn fold(&self, file: &File<'_>) -> Option<Const> {
        match self {
            Expr::Literal(span) => Some(Const::Str(file.str(*span).to_owned())),
            Expr::I64(v, _) | Expr::VerbatimI64(v, _) => Some(Const::I64(*v)),
            Expr::F64(v, _) | Expr::VerbatimF64(v, _) => Some(Const::F64(*v)),
            Expr::Bool(v, _) => Some(Const::Bool(*v)),
            Expr::Nil(_) => Some(Const::Nil),
            Expr::Sym(span) => Some(Const::Sym(*span)),
            Expr::Concat { exprs, .. } => {
                let mut acc = String::new();
                for expr in exprs.iter() {
                    acc.push_str(&expr.fold(file)?.to_string(file)?);
                }
                Some(Const::Str(acc))
            }
            Expr::Escape(v, _) => Some(Const::Str(v.to_string())),
            Expr::BinConcat { exprs, .. } => {
                let mut acc = Vec::new();
                for expr in exprs.iter() {
                    match expr {
                        Expr::Literal(span) => acc.extend_from_slice(file.str(*span).as_bytes()),
                        Expr::EscapeByte(b, _) => acc.push(*b),
                        Expr::Escape(c, _) => acc.push(*c as u8),
                        other => acc.extend_from_slice(&other.fold(file)?.to_bin()?),
                    }
                }
                Some(Const::Bin(acc))
            }
            Expr::EscapeByte(b, _) => Some(Const::Bin(vec![*b])),
            Expr::Group { expr, .. } => expr.fold(file),
            _ => None,
        }
    }

    fn concat(acc: &mut Option<Span>, exprs: &mut Vec<Expr>, span: Span) {
        if let Some(existing) = acc {
            if existing.end == span.start {
                existing.end = span.end
            } else {
                exprs.push(Expr::Literal(*existing));
                *acc = Some(span)
            }
        } else {
            *acc = Some(span)
        }
    }

    pub(crate) fn optimize(self) -> Self {
        match self {
            Expr::Concat {
                exprs,
                delim_span,
                arg: external,
            } if !exprs.is_empty() => {
                let mut new_exprs = vec![];
                let mut exprs = VecDeque::from(exprs);
                let mut acc: Option<Span> = None;
                while let Some(next) = exprs.pop_front() {
                    match next {
                        Expr::Concat {
                            exprs: subexprs,
                            delim_span: None,
                            arg: subexternal,
                        } if subexternal == external => {
                            for expr in subexprs.into_iter().rev() {
                                exprs.push_front(expr)
                            }
                        }
                        Expr::Literal(span) => Self::concat(&mut acc, &mut new_exprs, span),
                        Expr::VerbatimI64(_, span) | Expr::VerbatimF64(_, span) if external => {
                            Self::concat(&mut acc, &mut new_exprs, span)
                        }
                        Expr::Sym(span) if external => Self::concat(
                            &mut acc,
                            &mut new_exprs,
                            span.before_left_char() | span.after_right_char(),
                        ),
                        other => {
                            if let Some(existing) = acc {
                                new_exprs.push(Expr::Literal(existing));
                                acc = None
                            }
                            new_exprs.push(other);
                        }
                    }
                }
                if let Some(existing) = acc {
                    new_exprs.push(Expr::Literal(existing));
                }
                Expr::Concat {
                    exprs: new_exprs,
                    delim_span,
                    arg: external,
                }
            }
            Expr::BinConcat { exprs, open, close } if !exprs.is_empty() => {
                let mut new_exprs = vec![];
                let mut exprs = VecDeque::from(exprs);
                let mut acc: Option<Span> = None;
                while let Some(next) = exprs.pop_front() {
                    match next {
                        Expr::BinConcat {
                            exprs: subexprs, ..
                        } => {
                            for expr in subexprs.into_iter().rev() {
                                exprs.push_front(expr)
                            }
                        }
                        Expr::Literal(span) => Self::concat(&mut acc, &mut new_exprs, span),
                        other => {
                            if let Some(existing) = acc {
                                new_exprs.push(Expr::Literal(existing));
                                acc = None;
                            }
                            new_exprs.push(other);
                        }
                    }
                }
                if let Some(existing) = acc {
                    new_exprs.push(Expr::Literal(existing));
                }
                Expr::BinConcat {
                    exprs: new_exprs,
                    open,
                    close,
                }
            }
            _ => self,
        }
    }

    /// Recursively analyze this expression for side effects.
    /// Returns a classification indicating the likelihood of side effects.
    pub(crate) fn side_effect(&self) -> SideEffect {
        match self {
            // Literals - definitely no side effects
            Expr::Literal(_)
            | Expr::I64(_, _)
            | Expr::VerbatimI64(_, _)
            | Expr::F64(_, _)
            | Expr::VerbatimF64(_, _)
            | Expr::Bool(_, _)
            | Expr::Nil(_)
            | Expr::Sym(_)
            | Expr::Escape(_, _)
            | Expr::EscapeByte(_, _) => SideEffect::None,

            // Variable lookup - no side effects but references a variable
            Expr::Ident(_) => SideEffect::VarRef,

            // Grouping - check inner expression
            Expr::Group { expr, .. } => expr.side_effect(),

            // Unary operations - unlikely side effect
            Expr::Unary { expr, .. } => expr.side_effect().unlikely(),

            // Binary operations - combine classifications, unlikely side effect
            Expr::Binary { exprs, .. } => {
                Self::combine_effects(exprs[0].side_effect(), exprs[1].side_effect()).unlikely()
            }

            // Field access - check object, mark as Unlikely (could invoke complex attribute getter)
            // Note: Discarded remains Discarded (pointless processing of side-effectful result)
            Expr::Get { object, .. } => object.side_effect().unlikely(),

            // Index operation - check both parts, unlikely side effect
            Expr::Index { exprs, .. } => {
                Self::combine_effects(exprs[0].side_effect(), exprs[1].side_effect()).unlikely()
            }

            // Function calls - definitely have side effects
            Expr::Call { .. } => SideEffect::Likely,

            // Lambda creation - pure operation
            Expr::Lambda { .. } => SideEffect::None,

            // Array literals - check all elements
            Expr::Array { elems, .. } => Self::combine_iter(elems.iter().map(|e| match e {
                ArrayElem::Single(s) => s.expr.side_effect(),
                // Expansion upgrades VarRef to Unlikely (overloadable iteration/unpack)
                ArrayElem::Expand(e) => e.expr.side_effect().unlikely(),
                // For comprehension: analyze iteratee and pattern
                ArrayElem::For(f) => {
                    // Iteratee VarRef upgrades to Unlikely (overloadable iteration)
                    f.expr
                        .as_ref()
                        .map(|e| e.side_effect())
                        .unwrap_or(SideEffect::Likely)
                        .unlikely()
                }
                ArrayElem::If(i) => {
                    // Cond VarRef upgrades to Unlikely (overloadable bool conversion)
                    let cond = i.tbranch.cond.side_effect().unlikely();
                    // Branches are accumulated, use combine_iter
                    let branch_effect = Self::combine_iter(
                        i.tbranch
                            .body
                            .iter()
                            .map(|e| match e {
                                ArrayElem::Single(s) => s.expr.side_effect(),
                                ArrayElem::Expand(e) => e.expr.side_effect().unlikely(),
                                _ => SideEffect::Likely,
                            })
                            .chain(i.else_branch.iter().flat_map(|(body, _)| {
                                body.iter().map(|e| match e {
                                    ArrayElem::Single(s) => s.expr.side_effect(),
                                    ArrayElem::Expand(e) => e.expr.side_effect().unlikely(),
                                    _ => SideEffect::Likely,
                                })
                            })),
                    );
                    Self::combine_effects(cond, branch_effect)
                }
            })),

            // Dict literals - check all elements
            Expr::Dict { elems, .. } => Self::combine_iter(elems.iter().map(|e| match e {
                DictElem::Single(s) => s.expr.side_effect(),
                DictElem::Key(k) => k.expr.side_effect(),
                // Pair accumulates key and value effects, not binary combining
                DictElem::Pair(p) => {
                    Self::combine_effects(p.key.side_effect(), p.value.side_effect())
                }
                // Expansion upgrades VarRef to Unlikely (overloadable iteration/unpack)
                DictElem::Expand(e) => e.expr.side_effect().unlikely(),
                // For comprehension: analyze iteratee and pattern
                DictElem::For(f) => {
                    // Iteratee VarRef upgrades to Unlikely (overloadable iteration)
                    f.expr
                        .as_ref()
                        .map(|e| e.side_effect())
                        .unwrap_or(SideEffect::Likely)
                        .unlikely()
                }
                DictElem::If(i) => {
                    // Cond VarRef upgrades to Unlikely (overloadable bool conversion)
                    let cond = i.tbranch.cond.side_effect().unlikely();
                    // Branches are accumulated, use combine_iter
                    let branch_effect = Self::combine_iter(
                        i.tbranch
                            .body
                            .iter()
                            .map(|e| match e {
                                DictElem::Single(s) => s.expr.side_effect(),
                                DictElem::Key(k) => k.expr.side_effect(),
                                DictElem::Pair(p) => Self::combine_iter(
                                    [p.key.side_effect(), p.value.side_effect()].into_iter(),
                                ),
                                DictElem::Expand(e) => e.expr.side_effect().unlikely(),
                                _ => SideEffect::Likely,
                            })
                            .chain(i.else_branch.iter().flat_map(|(body, _)| {
                                body.iter().map(|e| match e {
                                    DictElem::Single(s) => s.expr.side_effect(),
                                    DictElem::Key(k) => k.expr.side_effect(),
                                    DictElem::Pair(p) => Self::combine_iter(
                                        [p.key.side_effect(), p.value.side_effect()].into_iter(),
                                    ),
                                    DictElem::Expand(e) => e.expr.side_effect().unlikely(),
                                    _ => SideEffect::Likely,
                                })
                            })),
                    );
                    Self::combine_effects(cond, branch_effect)
                }
            })),

            // String concatenation - upgrade VarRef to Unlikely (overloadable string conversion)
            Expr::Concat { exprs, .. } => {
                Self::combine_iter(exprs.iter().map(|e| e.side_effect().unlikely()))
            }

            // Binary concatenation - similar to string concatenation
            Expr::BinConcat { exprs, .. } => {
                Self::combine_iter(exprs.iter().map(|e| e.side_effect().unlikely()))
            }

            // Error sentinel - treat as maybe
            Expr::Error => SideEffect::Unlikely,
        }
    }

    /// Combine two side effect classifications.
    fn combine_effects(lhs: SideEffect, rhs: SideEffect) -> SideEffect {
        match (lhs, rhs) {
            // Both are pure constants
            (SideEffect::None, SideEffect::None) => SideEffect::None,
            // Discarded propagates
            (SideEffect::Discarded, _) | (_, SideEffect::Discarded) => SideEffect::Discarded,
            // Likely gets downgraded to Discarded when combined with anything
            (SideEffect::Likely, _) | (_, SideEffect::Likely) => SideEffect::Discarded,
            // If either is Unlikely, result is Unlikely
            (SideEffect::Unlikely, _) | (_, SideEffect::Unlikely) => SideEffect::Unlikely,
            // If either references a variable, result references a variable
            (SideEffect::VarRef, _) | (_, SideEffect::VarRef) => SideEffect::VarRef,
        }
    }

    /// Combine multiple side effect classifications.
    fn combine_iter(iter: impl Iterator<Item = SideEffect>) -> SideEffect {
        iter.fold(SideEffect::None, Self::combine_effects)
    }
}

impl Node for Expr {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            Expr::Literal(span) => visit.token(Token::Literal, *span, None),
            Expr::Ident(ident) => ident.accept(visit),
            Expr::I64(_, span) => visit.token(Token::Number, *span, None),
            Expr::VerbatimI64(_, span) => visit.token(Token::Number, *span, None),
            Expr::F64(_, span) => visit.token(Token::Number, *span, None),
            Expr::VerbatimF64(_, span) => visit.token(Token::Number, *span, None),
            Expr::Bool(_, span) => visit.token(Token::Constant, *span, None),
            Expr::Nil(span) => visit.token(Token::Constant, *span, None),
            Expr::Sym(span) => {
                visit.token(Token::Delim, span.before_left_char(), None)?;
                visit.token(Token::Constant, *span, None)?;
                visit.token(Token::Delim, span.after_right_char(), None)
            }
            Expr::Concat {
                exprs, delim_span, ..
            } => {
                if let Some(delim_span) = delim_span {
                    visit.token(Token::StringDelim, delim_span.left_char(), None)?;
                }
                exprs.accept(visit)?;
                if let Some(delim_span) = delim_span {
                    visit.token(Token::StringDelim, delim_span.right_char(), None)?
                }
                ControlFlow::Continue(())
            }
            Expr::Escape(_, span) => visit.token(Token::Escape, *span, None),
            Expr::BinConcat { exprs, open, close } => {
                visit.token(Token::StringDelim, *open, None)?;
                exprs.accept(visit)?;
                visit.token(Token::StringDelim, *close, None)
            }
            Expr::EscapeByte(_, span) => visit.token(Token::Escape, *span, None),
            Expr::Group { expr, delim } => {
                match delim {
                    None => (),
                    Some(GroupDelim::Paren(span)) => {
                        visit.token(Token::Delim, span.left_char(), None)?;
                        visit.token(Token::Delim, span.right_char(), None)?;
                    }
                    Some(GroupDelim::RawQuotes(start, end)) => {
                        visit.token(Token::StringDelim, *start, None)?;
                        visit.token(Token::StringDelim, *end, None)?;
                    }
                    Some(GroupDelim::Dollar(span)) => visit.token(Token::Sigil, *span, None)?,
                }
                visit.node(&**expr)
            }
            Expr::Unary { expr, op_span, .. } => {
                visit.token(Token::Operator, *op_span, None)?;
                visit.node(&**expr)
            }
            Expr::Binary { exprs, op_span, .. } => {
                visit.node(&exprs[0])?;
                visit.token(Token::Operator, *op_span, None)?;
                visit.node(&exprs[1])
            }
            Expr::Call { arg0, args, delim } => {
                match delim {
                    None => (),
                    Some(GroupDelim::Paren(span)) => {
                        visit.token(Token::Delim, span.left_char(), None)?;
                        visit.token(Token::Delim, span.right_char(), None)?;
                    }
                    Some(GroupDelim::RawQuotes(start, end)) => {
                        visit.token(Token::StringDelim, *start, None)?;
                        visit.token(Token::StringDelim, *end, None)?;
                    }
                    Some(GroupDelim::Dollar(span)) => visit.token(Token::Delim, *span, None)?,
                }
                visit.node(&**arg0)?;
                args.accept(visit)
            }
            Expr::Lambda { func, do_span } => {
                if let Some(do_span) = do_span {
                    visit.token(Token::Keyword, *do_span, None)?;
                }
                func.accept(visit)
            }
            Expr::Get {
                object,
                field,
                dot_span,
            } => {
                visit.node(&**object)?;
                visit.token(Token::Operator, *dot_span, None)?;
                match field {
                    GetVariant::Normal(span) => visit.token(Token::Field, *span, None),
                    GetVariant::SpecialMethod {
                        span, paren_span, ..
                    } => {
                        visit.token(Token::Keyword, *span, None)?;
                        visit.token(Token::Delim, paren_span.left_char(), None)?;
                        visit.token(Token::Delim, paren_span.right_char(), None)
                    }
                    GetVariant::Private { span, .. } => visit.token(Token::Field, *span, None),
                }
            }
            Expr::Index {
                bracket_span,
                exprs,
            } => {
                visit.node(&exprs[0])?;
                visit.token(Token::Delim, bracket_span.left_char(), None)?;
                visit.node(&exprs[1])?;
                visit.token(Token::Delim, bracket_span.right_char(), None)
            }
            Expr::Array {
                bracket_span,
                elems,
            } => {
                if let Some(bracket_span) = bracket_span {
                    visit.token(Token::Delim, bracket_span.left_char(), None)?;
                }
                elems.accept(visit)?;
                if let Some(bracket_span) = bracket_span {
                    visit.token(Token::Delim, bracket_span.right_char(), None)?;
                }
                ControlFlow::Continue(())
            }
            Expr::Dict { brace_span, elems } => {
                if let Some(brace_span) = brace_span {
                    visit.token(Token::Delim, brace_span.left_char(), None)?;
                }
                elems.accept(visit)?;
                if let Some(brace_span) = brace_span {
                    visit.token(Token::Delim, brace_span.right_char(), None)?;
                }
                ControlFlow::Continue(())
            }
            Expr::Error => ControlFlow::Continue(()),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            Expr::Literal(_) => NodeKind::Literal,
            Expr::Ident(_) => NodeKind::Ident,
            Expr::I64(_, _) => NodeKind::I64,
            Expr::VerbatimI64(_, _) => NodeKind::VerbatimI64,
            Expr::F64(_, _) => NodeKind::F64,
            Expr::VerbatimF64(_, _) => NodeKind::VerbatimF64,
            Expr::Bool(_, _) => NodeKind::Bool,
            Expr::Nil(_) => NodeKind::Nil,
            Expr::Sym(_) => NodeKind::Sym,
            Expr::Concat { .. } => NodeKind::Concat,
            Expr::Escape(_, _) => NodeKind::Escape,
            Expr::BinConcat { .. } => NodeKind::BinConcat,
            Expr::EscapeByte(_, _) => NodeKind::EscapeByte,
            Expr::Group { .. } => NodeKind::Group,
            Expr::Unary { .. } => NodeKind::Unary,
            Expr::Binary { .. } => NodeKind::Binary,
            Expr::Call { .. } => NodeKind::Call,
            Expr::Lambda { .. } => NodeKind::Lambda,
            Expr::Get { .. } => NodeKind::Field,
            Expr::Index { .. } => NodeKind::Index,
            Expr::Array { .. } => NodeKind::Array,
            Expr::Dict { .. } => NodeKind::Dict,
            Expr::Error => NodeKind::Error,
        }
    }
}

impl Expr {
    pub(crate) fn into_lvalue(self) -> Result<LValue, Expr> {
        Ok(match self {
            Expr::Ident(ident) => LValue::Ident(ident),
            Expr::Get {
                object,
                dot_span,
                field: GetVariant::Normal(field),
            } => LValue::Field {
                object,
                field,
                dot_span,
            },
            Expr::Get {
                object,
                dot_span,
                field: GetVariant::Private { span, res },
            } => LValue::PrivateField {
                object,
                field: span,
                dot_span,
                res,
            },
            Expr::Index {
                bracket_span,
                exprs,
            } => LValue::Index {
                bracket_span,
                exprs,
            },
            other => return Err(other),
        })
    }
}

pub(crate) enum LValue {
    Ident(Ident),
    Field {
        object: Box<Expr>,
        field: Span,
        dot_span: Span,
    },
    PrivateField {
        object: Box<Expr>,
        field: Span,
        dot_span: Span,
        res: Option<sym::Id>,
    },
    Index {
        bracket_span: Span,
        exprs: Box<[Expr; 2]>,
    },
}

impl Node for LValue {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            LValue::Ident(ident) => visit.node(ident),
            LValue::Field {
                object,
                field,
                dot_span,
            } => {
                visit.node(&**object)?;
                visit.token(Token::Operator, *dot_span, None)?;
                visit.token(Token::Field, *field, None)
            }
            LValue::PrivateField {
                object,
                field,
                dot_span,
                ..
            } => {
                visit.node(&**object)?;
                visit.token(Token::Operator, *dot_span, None)?;
                visit.token(Token::Field, *field, None)
            }
            LValue::Index {
                bracket_span,
                exprs,
            } => {
                visit.node(&exprs[0])?;
                visit.token(Token::Delim, bracket_span.left_char(), None)?;
                visit.node(&exprs[1])?;
                visit.token(Token::Delim, bracket_span.right_char(), None)
            }
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            LValue::Ident(_) => NodeKind::Ident,
            LValue::Field { .. } => NodeKind::Field,
            LValue::PrivateField { .. } => NodeKind::Field,
            LValue::Index { .. } => NodeKind::Index,
        }
    }
}

pub(crate) struct Key {
    pub(crate) key_span: Span,
    pub(crate) colon_span: Span,
    pub(crate) delim_span: Option<Span>,
    pub(crate) expr: Expr,
}

impl Node for Key {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Key, self.key_span, None)?;
        visit.token(Token::Delim, self.colon_span, None)?;
        visit.node(&self.expr)?;
        if let Some(delim_span) = &self.delim_span {
            visit.token(Token::Delim, *delim_span, None)?
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Key
    }
}

pub(crate) enum Arg {
    Pos(Single),
    Key(Key),
    DynamicKey(Pair),
    Expand(Expand),
    For(For<ExprBody<Arg>>),
    If(If<Vec<Arg>>),
}

impl Node for Arg {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            Arg::Pos(expr) => expr.accept(visit),
            Arg::Expand(node) => node.accept(visit),
            Arg::Key(node) => node.accept(visit),
            Arg::DynamicKey(node) => node.accept(visit),
            Arg::For(node) => node.accept(visit),
            Arg::If(node) => node.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            Arg::Pos(expr) => expr.kind(),
            Arg::Key(key) => key.kind(),
            Arg::DynamicKey(pair) => pair.kind(),
            Arg::Expand(expand) => expand.kind(),
            Arg::For(node) => node.kind(),
            Arg::If(node) => node.kind(),
        }
    }
}

pub(crate) struct ParamDefault {
    pub(crate) delim_span: Span,
    pub(crate) expr: Expr,
    pub(crate) fold: Option<Const>,
}

pub(crate) enum Param {
    Pos {
        ident: Ident,
        default: Option<ParamDefault>,
    },
    Key {
        key_span: Span,
        ident: Ident,
        default: Option<ParamDefault>,
    },
    ConstKey {
        key_expr: Expr,
        key_const: Const,
        ident: Ident,
        default: Option<ParamDefault>,
        colon_span: Span,
    },
    Rest {
        ellipsis_span: Span,
        ident: Option<Ident>,
    },
}

impl Node for Param {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            Param::Pos { ident, default } => {
                visit.token(
                    Token::Variable,
                    ident.span,
                    ident.res.as_ref().map(|r| r.origin),
                )?;
                if let Some(default) = default {
                    visit.token(Token::Delim, default.delim_span, None)?;
                    visit.node(&default.expr)?;
                }
                ControlFlow::Continue(())
            }
            Param::Key {
                key_span,
                ident,
                default,
            } => {
                visit.token(Token::Key, *key_span, None)?;
                visit.token(Token::Delim, key_span.after_right_char(), None)?;
                visit.token(
                    Token::Variable,
                    ident.span,
                    ident.res.as_ref().map(|r| r.origin),
                )?;
                if let Some(default) = default {
                    visit.token(Token::Delim, default.delim_span, None)?;
                    visit.node(&default.expr)?;
                }
                ControlFlow::Continue(())
            }
            Param::ConstKey {
                key_expr,
                ident,
                default,
                colon_span,
                ..
            } => {
                visit.node(key_expr)?;
                visit.token(Token::Delim, *colon_span, None)?;
                visit.token(
                    Token::Variable,
                    ident.span,
                    ident.res.as_ref().map(|r| r.origin),
                )?;
                if let Some(default) = default {
                    visit.token(Token::Delim, default.delim_span, None)?;
                    visit.node(&default.expr)?;
                }
                ControlFlow::Continue(())
            }
            Param::Rest {
                ellipsis_span,
                ident,
            } => {
                visit.token(Token::Sigil, *ellipsis_span, None)?;
                if let Some(ident) = ident {
                    visit.token(
                        Token::Variable,
                        ident.span,
                        ident.res.as_ref().map(|r| r.origin),
                    )?;
                }
                ControlFlow::Continue(())
            }
        }
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Param
    }
}

impl<T: Node, U> Node for U
where
    U: Deref<Target = [T]>,
{
    const TRANSPARENT: bool = true;

    fn span(&self) -> Span {
        if self.deref().is_empty() {
            return Span::INVALID;
        }
        self.deref().first().unwrap().span() | self.deref().last().unwrap().span()
    }

    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        for node in self.as_ref().iter() {
            node.trans(visit)?
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        unreachable!()
    }
}

pub(crate) enum Pattern {
    Ident(Ident),
    Unpack(Vec<Param>),
}

impl Node for Pattern {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            Pattern::Ident(ident) => ident.accept(visit),
            Pattern::Unpack(params) => params.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            Pattern::Ident(ident) => ident.kind(),
            Pattern::Unpack(_) => NodeKind::Pattern,
        }
    }
}

pub(crate) struct Let {
    pub(crate) bind: Pattern,
    pub(crate) rhs: PrimStmt,
    pub(crate) let_span: Span,
    pub(crate) equal_span: Span,
    pub(crate) pub_span: Option<Span>,
}

impl Node for Let {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        if let Some(span) = self.pub_span {
            visit.token(Token::Keyword, span, None)?;
        }
        visit.token(Token::Keyword, self.let_span, None)?;
        visit.node(&self.bind)?;
        visit.token(Token::Operator, self.equal_span, None)?;
        visit.node(&self.rhs)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Let
    }
}

pub(crate) struct Bind {
    pub(crate) bind: Pattern,
    pub(crate) expr: Expr,
    pub(crate) bind_span: Span,
}

impl Node for Bind {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.bind_span, None)?;
        visit.node(&self.expr)?;
        visit.node(&self.bind)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Bind
    }
}

pub(crate) struct Assign {
    pub(crate) lhs: LValue,
    pub(crate) rhs: PrimStmt,
    pub(crate) equal_span: Span,
}

impl Node for Assign {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.node(&self.lhs)?;
        visit.token(Token::Operator, self.equal_span, None)?;
        visit.node(&self.rhs)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Assign
    }
}

pub(crate) struct While {
    pub(crate) cond: Expr,
    pub(crate) body: Block,
    pub(crate) while_span: Span,
}

impl Node for While {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.while_span, None)?;
        visit.node(&self.cond)?;
        visit.node(&self.body)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::While
    }
}

pub(crate) enum ImportItem {
    AsIs {
        bind: Ident,
        delim_span: Span,
    },
    Renamed {
        item: Span,
        bind: Ident,
        delim_span: Span,
    },
}

impl Node for ImportItem {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            ImportItem::Renamed {
                item,
                bind,
                delim_span,
            } => {
                visit.token(Token::ModuleItem, *item, None)?;
                visit.token(Token::Delim, *delim_span, None)?;
                visit.node(bind)
            }
            ImportItem::AsIs { bind, delim_span } => {
                visit.token(Token::Delim, *delim_span, None)?;
                visit.node(bind)
            }
        }
    }

    fn kind(&self) -> NodeKind {
        NodeKind::ImportItem
    }
}

pub(crate) enum ImportElement {
    ModuleAsIs {
        module: Span,
        bind: Ident,
        insert: bool,
    },
    ModuleRenamed {
        module: Span,
        bind: Ident,
        delim_span: Span,
    },
    Items {
        module: Span,
        items: Vec<ImportItem>,
    },
}

impl Node for ImportElement {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            ImportElement::ModuleAsIs { module, bind, .. } => {
                visit.token(Token::ModuleName, *module, None)?;
                visit.node(bind)
            }
            ImportElement::ModuleRenamed {
                module,
                bind,
                delim_span,
            } => {
                visit.token(Token::ModuleName, *module, None)?;
                visit.token(Token::Delim, *delim_span, None)?;
                visit.node(bind)
            }
            ImportElement::Items { module, items } => {
                visit.token(Token::ModuleName, *module, None)?;
                items.accept(visit)
            }
        }
    }

    fn kind(&self) -> NodeKind {
        NodeKind::ImportItem
    }
}

pub(crate) struct Import(pub(crate) Vec<ImportElement>, pub(crate) Span);

impl Node for Import {
    fn span(&self) -> Span {
        self.1 | self.0.last().as_ref().unwrap().span()
    }

    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.1, None)?;
        self.0.accept(visit)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::ImportItem
    }
}

pub(crate) struct Return {
    pub(crate) expr: Option<Expr>,
    pub(crate) span: Span,
    pub(crate) nl: Option<NlInfo>,
}

impl Node for Return {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.span, None)?;
        if let Some(expr) = &self.expr {
            visit.node(expr)?;
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Return
    }
}

pub(crate) struct Throw {
    pub(crate) expr: Expr,
    pub(crate) span: Span,
}

impl Node for Throw {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.span, None)?;
        visit.node(&self.expr)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Throw
    }
}

/// Guard node wrapping a statement that may contain non-local jumps from inner closures
pub(crate) struct NlGuard {
    pub(crate) body: Box<Stmt>,
    pub(crate) span: Span,
    pub(crate) has_break: bool,
    pub(crate) has_continue: bool,
    pub(crate) has_return: Option<Res>,
}

pub(crate) struct CatchHandler {
    pub(crate) class_expr: Option<Expr>,
    pub(crate) func: Function,
    pub(crate) catch_span: Span,
}

pub(crate) struct Try {
    pub(crate) body: Function,
    pub(crate) handlers: Vec<CatchHandler>,
    pub(crate) finally: Option<(Function, Span)>,
    pub(crate) try_span: Span,
}

impl Node for CatchHandler {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.catch_span, None)?;
        if let Some(class_expr) = &self.class_expr {
            visit.node(class_expr)?;
        }
        visit.node(&self.func)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Catch
    }
}

impl Node for Try {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.token(Token::Keyword, self.try_span, None)?;
        visit.node(&self.body)?;
        for handler in &self.handlers {
            visit.node(handler)?;
        }
        if let Some((finally_func, finally_span)) = &self.finally {
            visit.token(Token::Keyword, *finally_span, None)?;
            visit.node(finally_func)?;
        }
        ControlFlow::Continue(())
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Try
    }
}

pub(crate) enum PrimStmt {
    Expr(Expr),
    If(If<Block>),
    Try(Try),
}

impl Node for NlGuard {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        visit.node(&*self.body)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::NlGuard
    }
}

impl Node for PrimStmt {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            PrimStmt::Expr(expr) => expr.accept(visit),
            PrimStmt::If(node) => node.accept(visit),
            PrimStmt::Try(node) => node.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            PrimStmt::Expr(expr) => expr.kind(),
            PrimStmt::If(node) => node.kind(),
            PrimStmt::Try(node) => node.kind(),
        }
    }
}

pub(crate) enum DefVariant {
    Normal(Ident),
    Special(SpecialMethod, Span, Option<Res>),
}

pub(crate) struct Def {
    // Span of the `def` keyword
    pub(crate) def_span: Span,
    // Defined identifier or special method
    pub(crate) variant: DefVariant,
    // Function
    pub(crate) func: Function,
    pub(crate) pub_span: Option<Span>,
}

impl Node for Def {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        if let Some(span) = self.pub_span {
            visit.token(Token::Keyword, span, None)?;
        }
        visit.token(Token::Keyword, self.def_span, None)?;
        match &self.variant {
            DefVariant::Normal(ident) => visit.token(
                Token::Variable,
                ident.span,
                ident.res.as_ref().map(|r| r.origin),
            )?,
            DefVariant::Special(_, span, res) => {
                visit.token(Token::Keyword, *span, res.as_ref().map(|r| r.origin))?
            }
        }
        visit.node(&self.func)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Def
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum SpecialMethod {
    Init,
    Call,
    Unpack,
    Iter,
    Sink,
    Next,
    Put,
    Str,
    Dbg,
    Arg,
    Add,
    Sub,
    Rsub,
    Mul,
    Div,
    Rdiv,
    Ediv,
    Rediv,
    Mod,
    Rmod,
    Band,
    Bor,
    Bxor,
    Neg,
    Bnot,
    Eq,
    Lt,
    Bool,
    Index,
    Assign,
    Get,
    Set,
    Hash,
}

impl SpecialMethod {
    pub(crate) fn sym(&self) -> &'static str {
        match self {
            SpecialMethod::Init => "(init)",
            SpecialMethod::Call => "(call)",
            SpecialMethod::Unpack => "(unpack)",
            SpecialMethod::Iter => "(iter)",
            SpecialMethod::Sink => "(sink)",
            SpecialMethod::Next => "(next)",
            SpecialMethod::Put => "(put)",
            SpecialMethod::Str => "(str)",
            SpecialMethod::Dbg => "(dbg)",
            SpecialMethod::Arg => "(arg)",
            SpecialMethod::Add => "(add)",
            SpecialMethod::Sub => "(sub)",
            SpecialMethod::Rsub => "(rsub)",
            SpecialMethod::Mul => "(mul)",
            SpecialMethod::Div => "(div)",
            SpecialMethod::Rdiv => "(rdiv)",
            SpecialMethod::Ediv => "(ediv)",
            SpecialMethod::Rediv => "(rediv)",
            SpecialMethod::Mod => "(mod)",
            SpecialMethod::Rmod => "(rmod)",
            SpecialMethod::Band => "(band)",
            SpecialMethod::Bor => "(bor)",
            SpecialMethod::Bxor => "(bxor)",
            SpecialMethod::Neg => "(neg)",
            SpecialMethod::Bnot => "(bnot)",
            SpecialMethod::Eq => "(eq)",
            SpecialMethod::Lt => "(lt)",
            SpecialMethod::Bool => "(bool)",
            SpecialMethod::Index => "(index)",
            SpecialMethod::Assign => "(assign)",
            SpecialMethod::Get => "(get)",
            SpecialMethod::Set => "(set)",
            SpecialMethod::Hash => "(hash)",
        }
    }
}

pub(crate) struct Class {
    // Span of the `class` keyword
    pub(crate) class_span: Span,
    // Class name identifier
    pub(crate) ident: Ident,
    // Span of the `:` delimiter (if superclasses are present)
    pub(crate) colon_span: Option<Span>,
    // Superclass expressions (empty = no superclasses)
    pub(crate) super_exprs: Vec<Expr>,
    // Class body (block containing defs and lets)
    pub(crate) body: Block,
    pub(crate) pub_span: Option<Span>,
}

impl Node for Class {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        if let Some(span) = self.pub_span {
            visit.token(Token::Keyword, span, None)?;
        }
        visit.token(Token::Keyword, self.class_span, None)?;
        visit.token(
            Token::Variable,
            self.ident.span,
            self.ident.res.as_ref().map(|r| r.origin),
        )?;
        if let Some(colon_span) = self.colon_span {
            visit.token(Token::Delim, colon_span, None)?;
        }
        for super_expr in &self.super_exprs {
            visit.node(super_expr)?;
        }
        visit.node(&self.body)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Class
    }
}

pub(crate) enum Stmt {
    Assign(Assign),
    Bind(Bind),
    Break(Span, Option<NlInfo>),
    Class(Class),
    Continue(Span, Option<NlInfo>),
    Def(Def),
    For(For<Block>),
    Import(Import),
    Let(Let),
    NlGuard(NlGuard),
    Prim(PrimStmt),
    Return(Return),
    Throw(Throw),
    While(While),
}

impl Node for Stmt {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        match self {
            Stmt::Assign(node) => node.accept(visit),
            Stmt::Bind(node) => node.accept(visit),
            Stmt::Break(span, _) => visit.token(Token::Keyword, *span, None),
            Stmt::Class(node) => node.accept(visit),
            Stmt::Continue(span, _) => visit.token(Token::Keyword, *span, None),
            Stmt::Def(node) => node.accept(visit),
            Stmt::For(node) => node.accept(visit),
            Stmt::Import(import) => import.accept(visit),
            Stmt::Let(node) => node.accept(visit),
            Stmt::NlGuard(guard) => guard.accept(visit),
            Stmt::Prim(prim) => prim.accept(visit),
            Stmt::Return(node) => node.accept(visit),
            Stmt::Throw(node) => node.accept(visit),
            Stmt::While(node) => node.accept(visit),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            Stmt::Assign(_) => NodeKind::Assign,
            Stmt::Bind(_) => NodeKind::Bind,
            Stmt::Break(..) => NodeKind::Break,
            Stmt::Class(_) => NodeKind::Class,
            Stmt::Continue(..) => NodeKind::Continue,
            Stmt::Def(_) => NodeKind::Def,
            Stmt::For(_) => NodeKind::For,
            Stmt::Import(_) => NodeKind::Import,
            Stmt::Let(_) => NodeKind::Let,
            Stmt::NlGuard(guard) => guard.kind(),
            Stmt::Prim(prim) => prim.kind(),
            Stmt::Return(_) => NodeKind::Return,
            Stmt::Throw(_) => NodeKind::Throw,
            Stmt::While(_) => NodeKind::While,
        }
    }
}

pub(crate) struct Block {
    pub(crate) stmts: Vec<Stmt>,
    pub(crate) vars: Vec<Var>,
    pub(crate) repl: Option<Res>,
}

impl Node for Block {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        self.stmts.accept(visit)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Block
    }
}

impl Block {
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Stmt> {
        self.stmts.iter_mut()
    }
}

pub(crate) struct Function {
    pub(crate) params: Vec<Param>,
    pub(crate) body: Block,
}

impl Node for Function {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        self.params.accept(visit)?;
        visit.node(&self.body)
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Function
    }
}

pub struct Unit(pub(crate) Function);

impl Node for Unit {
    fn accept<'a, V: Visit>(&'a self, visit: &'a mut V) -> ControlFlow<V::Break> {
        self.0.accept(visit)
    }

    fn kind(&self) -> NodeKind {
        self.0.kind()
    }
}
