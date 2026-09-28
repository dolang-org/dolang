//! Expressions: owned trees, mirroring the AST. A node that is a checking rule
//! carries a [`RuleId`], which keys what flow learns about it; other nodes carry
//! nothing.

use super::{FuncId, Pattern, RuleId, VarId};
use crate::{
    lex::Op,
    source::Span,
    typeck::{
        elab::ModuleRef,
        r#type::{DeclId, Literal, MemberKey, SymbolId},
    },
};

pub(crate) struct Expr {
    pub(crate) kind: ExprKind,
    pub(crate) span: Span,
}

pub(crate) enum ExprKind {
    Literal(Literal),
    Float,
    Bin,
    /// A string built from parts, each of which may be any value
    Concat(Vec<Expr>),
    /// A binary string built from parts, each of which must be binary
    BinConcat {
        parts: Vec<Expr>,
        rule: RuleId,
    },
    /// A `t"..."` sequence: a `Fmt` of literal text and interpolations, each a
    /// [`ExprKind::FmtValue`] or [`ExprKind::FmtParam`]
    Fmt(Vec<Expr>),
    /// An interpolation: a `FmtValue` binding a value to a specification. A string
    /// formats it in place.
    FmtValue {
        value: Box<Expr>,
        spec: FmtSpec,
        rule: RuleId,
    },
    /// A `${#...}` interpolation: a `FmtParam`, which a `Fmt` fills later
    FmtParam {
        spec: FmtSpec,
        rule: RuleId,
    },
    /// A reference to a variable
    Var(VarId),
    /// A variable's value that lowering copies, spanned as the source it stands
    /// for, such as a statement's value, rather than a reference
    Copy(VarId),
    /// The class object a class statement defines, of type `Type[C]`
    Class(DeclId),
    /// An imported module, or an item of one
    Import {
        module: ModuleRef,
        item: Option<SymbolId>,
    },
    /// Instantiating a closure
    Lambda(FuncId),
    Call {
        callee: Box<Expr>,
        args: Vec<Item>,
        rule: RuleId,
    },
    /// A method call, which looks the method up and calls it in one rule
    Invoke {
        receiver: Box<Expr>,
        member: Member,
        args: Vec<Item>,
        rule: RuleId,
    },
    Get {
        object: Box<Expr>,
        member: Member,
        rule: RuleId,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
        rule: RuleId,
    },
    Unary {
        op: Op,
        operand: Box<Expr>,
        rule: RuleId,
    },
    /// Never `&&` or `||`, which are control flow
    Binary {
        op: Op,
        operands: Box<[Expr; 2]>,
        rule: RuleId,
    },
    Range {
        bounds: Box<[Option<Expr>; 2]>,
        rule: RuleId,
    },
    Collection {
        kind: Collection,
        items: Vec<Item>,
        rule: RuleId,
    },
    /// The strand's ambient input, iterated by a `for` with no iteratee
    AmbientInput,
    /// A value popped from the operand stack. The operands of a step or terminal pop
    /// bottom-up, in evaluation order.
    Operand,
    /// A value of the bottom type, standing in for a value that another edge
    /// supplies, such as the result a phantom return assigns
    Never,
    /// The namespace `import a.b` binds `a` to, which only a dotted path through it
    /// can be typed by
    Namespace,
    /// Recovery from an expression that failed to elaborate
    Error,
}

/// The parts of a format specification that are evaluated, its width and precision,
/// which must be `Int`s. The rest is constant.
pub(crate) struct FmtSpec {
    pub(crate) width: Option<Box<Expr>>,
    pub(crate) precision: Option<Box<Expr>>,
}

impl FmtSpec {
    fn walk<'a>(&'a self, visit: &mut impl FnMut(&'a Expr)) {
        [&self.width, &self.precision]
            .into_iter()
            .flatten()
            .for_each(|expr| expr.walk(visit));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Collection {
    Array,
    Dict,
    Tuple,
    Record,
}

/// A member of a receiver, by key. A private member names the class whose member it
/// is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) key: MemberKey,
    pub(crate) class: Option<DeclId>,
}

/// An item of an argument list or collection
pub(crate) enum Item {
    Pos(Expr),
    Key(SymbolId, Expr),
    Pair(Expr, Expr),
    Spread(Expr),
    /// A comprehension's loop: its items occur zero or more times. The iteratee and
    /// pattern are lowered to blocks before the item.
    For(Vec<Item>),
    /// A comprehension's filter: `then`'s items occur, or `else_`'s. The condition is
    /// lowered to blocks before the item.
    If {
        then: Vec<Item>,
        else_: Vec<Item>,
    },
}

/// What an assignment writes
pub(crate) enum Target {
    Var(VarId),
    Field {
        object: Expr,
        member: Member,
        rule: RuleId,
    },
    Index {
        object: Expr,
        index: Expr,
        rule: RuleId,
    },
}

impl Expr {
    /// Visit this expression and each one nested in it, parents first
    pub(crate) fn walk<'a>(&'a self, visit: &mut impl FnMut(&'a Expr)) {
        visit(self);
        match &self.kind {
            ExprKind::Concat(parts) | ExprKind::BinConcat { parts, .. } | ExprKind::Fmt(parts) => {
                parts.iter().for_each(|part| part.walk(visit))
            }
            ExprKind::FmtValue { value, spec, .. } => {
                value.walk(visit);
                spec.walk(visit);
            }
            ExprKind::FmtParam { spec, .. } => spec.walk(visit),
            ExprKind::Call { callee, args, .. } => {
                callee.walk(visit);
                Item::walk_all(args, visit);
            }
            ExprKind::Invoke { receiver, args, .. } => {
                receiver.walk(visit);
                Item::walk_all(args, visit);
            }
            ExprKind::Get { object, .. } => object.walk(visit),
            ExprKind::Index { object, index, .. } => {
                object.walk(visit);
                index.walk(visit);
            }
            ExprKind::Unary { operand, .. } => operand.walk(visit),
            ExprKind::Binary { operands, .. } => operands.iter().for_each(|expr| expr.walk(visit)),
            ExprKind::Range { bounds, .. } => {
                bounds.iter().flatten().for_each(|expr| expr.walk(visit))
            }
            ExprKind::Collection { items, .. } => Item::walk_all(items, visit),
            ExprKind::Literal(_)
            | ExprKind::Float
            | ExprKind::Bin
            | ExprKind::Var(_)
            | ExprKind::Copy(_)
            | ExprKind::Class(_)
            | ExprKind::Import { .. }
            | ExprKind::Lambda(_)
            | ExprKind::AmbientInput
            | ExprKind::Operand
            | ExprKind::Never
            | ExprKind::Namespace
            | ExprKind::Error => {}
        }
    }

    /// The rule it is, if it's one
    pub(crate) fn rule(&self) -> Option<RuleId> {
        match self.kind {
            ExprKind::Call { rule, .. }
            | ExprKind::Invoke { rule, .. }
            | ExprKind::Get { rule, .. }
            | ExprKind::Index { rule, .. }
            | ExprKind::Unary { rule, .. }
            | ExprKind::Binary { rule, .. }
            | ExprKind::Range { rule, .. }
            | ExprKind::Collection { rule, .. }
            | ExprKind::BinConcat { rule, .. }
            | ExprKind::FmtValue { rule, .. }
            | ExprKind::FmtParam { rule, .. } => Some(rule),
            _ => None,
        }
    }
}

impl Item {
    fn walk_all<'a>(items: &'a [Item], visit: &mut impl FnMut(&'a Expr)) {
        for item in items {
            match item {
                Item::Pos(expr) | Item::Key(_, expr) | Item::Spread(expr) => expr.walk(visit),
                Item::Pair(key, value) => {
                    key.walk(visit);
                    value.walk(visit);
                }
                Item::For(items) => Item::walk_all(items, visit),
                Item::If { then, else_ } => {
                    Item::walk_all(then, visit);
                    Item::walk_all(else_, visit);
                }
            }
        }
    }
}

impl Pattern {
    /// Visit the expressions in its constant keys
    pub(crate) fn walk<'a>(&'a self, visit: &mut impl FnMut(&'a Expr)) {
        let Pattern::Unpack(items) = self else {
            return;
        };
        for item in items {
            if let super::PatternKey::ConstKey(key) = &item.key {
                key.walk(visit);
            }
        }
    }

    /// The variables it binds
    pub(crate) fn vars(&self) -> impl Iterator<Item = VarId> {
        let vars: Vec<VarId> = match self {
            Pattern::Bind(var) => vec![*var],
            Pattern::Unpack(items) => items.iter().filter_map(|item| item.var).collect(),
        };
        vars.into_iter()
    }
}
