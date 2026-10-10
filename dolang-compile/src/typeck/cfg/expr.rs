//! Expressions: owned trees, mirroring the AST.

use std::slice;

use dolang_util::alias;

use super::{FuncId, Pattern, VarId};
use crate::{
    lex::Op,
    source::Span,
    typeck::{
        elab::ModuleRef,
        r#type::{DeclId, Literal, MemberKey, SymbolId, TypeId},
    },
};

pub(crate) struct Expr {
    pub(crate) kind: ExprKind,
    pub(crate) span: Span,
}

pub(crate) enum ExprKind {
    /// A runtime class test; its branch assumptions carry the narrowing.
    TypeTest {
        value: Box<Expr>,
        #[cfg_attr(
            not(feature = "debug"),
            expect(dead_code, reason = "read by the debug dump")
        )]
        class: Option<DeclId>,
    },
    /// A cast, `(value @ ty)`, or an unchecked one, `(value !@ ty)`, whose result
    /// is `ty`
    Cast {
        value: Box<Expr>,
        ty: TypeId,
        checked: bool,
    },
    Literal(Literal),
    Float,
    Bin,
    /// A string built from parts, each of which may be any value
    Concat(alias::Box<[Expr]>),
    /// A binary string built from parts, each of which must be binary
    BinConcat {
        parts: alias::Box<[Expr]>,
    },
    /// A `t"..."` sequence: a `Fmt` of literal text and interpolations, each a
    /// [`ExprKind::FmtValue`] or [`ExprKind::FmtParam`]
    Fmt(alias::Box<[Expr]>),
    /// An interpolation: a `FmtValue` binding a value to a specification. A string
    /// formats it in place.
    FmtValue {
        value: Box<Expr>,
        spec: FmtSpec,
    },
    /// A `${#...}` interpolation: a `FmtParam`, which a `Fmt` fills later
    FmtParam {
        name: Literal,
        name_span: Span,
        spec: FmtSpec,
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
        args: alias::Box<[Item]>,
    },
    /// A method call, which looks the method up and calls it in one rule
    Invoke {
        receiver: Box<Expr>,
        member: Member,
        args: alias::Box<[Item]>,
    },
    Get {
        object: Box<Expr>,
        member: Member,
    },
    /// Indexing: `(index)` passed the index
    Index {
        object: Box<Expr>,
        index: Box<Item>,
    },
    Unary {
        op: Op,
        operand: Box<Item>,
    },
    /// Never `&&` or `||`, which are control flow
    Binary {
        op: Op,
        operands: Box<[Item; 2]>,
    },
    Range {
        bounds: Box<[Option<Expr>; 2]>,
    },
    Collection {
        kind: Collection,
        items: alias::Box<[Item]>,
    },
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
    /// pattern are lowered to blocks before the item. `span` is its `for`'s.
    For {
        items: alias::Box<[Item]>,
        span: Span,
    },
    /// A comprehension's filter: `then`'s items occur, or `else_`'s. The condition is
    /// lowered to blocks before the item. `span` is its `if`'s.
    If {
        then: alias::Box<[Item]>,
        else_: alias::Box<[Item]>,
        span: Span,
    },
}

/// What an assignment writes, and the value it writes. A setter or `(set)` is
/// passed the value, and `(assign)` the index and the value, as arguments.
pub(crate) enum Target {
    Var {
        var: VarId,
        value: Expr,
    },
    Field {
        object: Expr,
        member: Member,
        value: Box<Item>,
    },
    Index {
        object: Expr,
        args: Box<[Item; 2]>,
    },
    /// A module's member, through an import path to the module, spanning the path
    Import {
        module: ModuleRef,
        item: SymbolId,
        span: Span,
        value: Expr,
    },
}

impl Target {
    /// The value written
    pub(crate) fn value(&self) -> &Expr {
        match self {
            Target::Var { value, .. } | Target::Import { value, .. } => value,
            Target::Field { value, .. } => value.pos(),
            Target::Index { args, .. } => args[1].pos(),
        }
    }
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
            ExprKind::TypeTest { value, .. } | ExprKind::Cast { value, .. } => value.walk(visit),
            ExprKind::Get { object, .. } => object.walk(visit),
            ExprKind::Index { object, index, .. } => {
                object.walk(visit);
                Item::walk_all(slice::from_ref(index), visit);
            }
            ExprKind::Unary { operand, .. } => Item::walk_all(slice::from_ref(operand), visit),
            ExprKind::Binary { operands, .. } => Item::walk_all(&operands[..], visit),
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
            | ExprKind::Operand
            | ExprKind::Never
            | ExprKind::Namespace
            | ExprKind::Error => {}
        }
    }

    /// Whether it's a checking rule
    pub(crate) fn is_rule(&self) -> bool {
        matches!(
            self.kind,
            ExprKind::TypeTest { .. }
                | ExprKind::Call { .. }
                | ExprKind::Invoke { .. }
                | ExprKind::Get { .. }
                | ExprKind::Index { .. }
                | ExprKind::Unary { .. }
                | ExprKind::Binary { .. }
                | ExprKind::Range { .. }
                | ExprKind::Collection { .. }
                | ExprKind::BinConcat { .. }
                | ExprKind::FmtValue { .. }
                | ExprKind::FmtParam { .. }
        )
    }
}

impl Item {
    /// An operand's expression. Lowering makes every operand positional.
    pub(crate) fn pos(&self) -> &Expr {
        match self {
            Item::Pos(expr) => expr,
            _ => unreachable!("a positional operand"),
        }
    }

    fn walk_all<'a>(items: &'a [Item], visit: &mut impl FnMut(&'a Expr)) {
        for item in items {
            match item {
                Item::Pos(expr) | Item::Key(_, expr) | Item::Spread(expr) => expr.walk(visit),
                Item::Pair(key, value) => {
                    key.walk(visit);
                    value.walk(visit);
                }
                Item::For { items, .. } => Item::walk_all(items, visit),
                Item::If { then, else_, .. } => {
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
