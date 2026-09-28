//! Expressions: their types in a state, and the class and import values they name.

use std::collections::VecDeque;

use crate::source::Span;

use super::{At, Flow, State, problem::Problem};
use crate::typeck::{
    cfg::{Expr, ExprKind, FuncId, FuncKind, Item},
    elab::{Designated, ModuleRef, Referent, Target},
    r#type::{
        Argument, BoundRef, DeclId, DeclKind, Intrinsic, Kind, SymbolId, Type, TypeId, UnitId,
    },
};

impl Flow<'_, '_> {
    /// An expression's type, popping an operand for each hole in evaluation order
    pub(super) fn eval(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> TypeId {
        self.expect(at, state, operands, expr, None)
    }

    /// An expression's type, where a type is expected of it that the fixed point
    /// can't revise: a local's annotation, or the parameter of a callee known from
    /// its signature alone. The expression's rule may solve toward it.
    pub(super) fn expect(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let unknown = self.db.unknown();
        match &expr.kind {
            ExprKind::Literal(literal) => self.literal(literal),
            ExprKind::Float => self.designated_type(Designated::Float),
            ExprKind::Bin => self.designated_type(Designated::Bin),
            ExprKind::Concat(parts) => {
                for part in parts {
                    self.eval(at, state, operands, part);
                }
                self.intrinsic(Intrinsic::Str)
            }
            ExprKind::BinConcat { parts, .. } => self.bin_concat(at, state, operands, parts),
            ExprKind::Fmt(parts) => {
                let bottom = self.db.bottom();
                let mut never = false;
                for part in parts {
                    never |= self.eval(at, state, operands, part) == bottom;
                }
                match never {
                    true => bottom,
                    false => self.designated_type(Designated::Fmt),
                }
            }
            ExprKind::FmtValue { .. } | ExprKind::FmtParam { .. } => {
                self.fmt(at, state, operands, expr)
            }
            &ExprKind::Var(var) => {
                let fact = self.read(at, state, var);
                self.record(expr.span, fact);
                if fact.unassigned && self.observing() && expr.span != Span::INVALID {
                    self.problem(Problem::Unassigned {
                        span: expr.span,
                        name: self.tables.text(self.unit, expr.span).to_owned(),
                        definitely: fact.ty == self.db.bottom(),
                    });
                }
                fact.ty
            }
            &ExprKind::Copy(var) => self.read(at, state, var).ty,
            &ExprKind::Class(decl) => self.class_object(decl),
            ExprKind::Import { module, item } => self.import(module, *item),
            &ExprKind::Lambda(func) => self.lambda(at, func),
            ExprKind::Call { .. } => self.call(at, state, operands, expr, expected),
            ExprKind::Invoke { receiver, args, .. } => {
                self.eval(at, state, operands, receiver);
                self.items(at, state, operands, args);
                unknown
            }
            ExprKind::Get { object, .. } => {
                self.eval(at, state, operands, object);
                unknown
            }
            ExprKind::Index { object, index, .. } => {
                self.eval(at, state, operands, object);
                self.eval(at, state, operands, index);
                unknown
            }
            ExprKind::Unary { operand, .. } => {
                self.eval(at, state, operands, operand);
                unknown
            }
            ExprKind::Binary { operands: pair, .. } => {
                for operand in pair.iter() {
                    self.eval(at, state, operands, operand);
                }
                unknown
            }
            ExprKind::Range { bounds, .. } => {
                for bound in bounds.iter().flatten() {
                    self.eval(at, state, operands, bound);
                }
                unknown
            }
            ExprKind::Collection { .. } => self.collection(at, state, operands, expr, expected),
            ExprKind::Operand => operands.pop_front().expect("an operand for each hole"),
            ExprKind::Never => self.db.bottom(),
            ExprKind::AmbientInput | ExprKind::Namespace | ExprKind::Error => unknown,
        }
    }

    fn items(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
    ) {
        for item in items {
            match item {
                Item::Pos(value) | Item::Key(_, value) | Item::Spread(value) => {
                    self.eval(at, state, operands, value);
                }
                Item::Pair(key, value) => {
                    self.eval(at, state, operands, key);
                    self.eval(at, state, operands, value);
                }
                Item::For { items, .. } => self.items(at, state, operands, items),
                Item::If { then, else_, .. } => {
                    self.items(at, state, operands, then);
                    self.items(at, state, operands, else_);
                }
            }
        }
    }

    /// Instantiate a closure outside a rule that types it. Nothing here gives a
    /// `do` block's parameters and channels anything; once the analysis is stuck,
    /// those still bottom are dynamic (see [`Flow::analyze`]). Its value is
    /// its declared type under its rigids, with the result its variable joins, if
    /// that's left to it. A nested def's value is its declared type, unless it's
    /// nested in a generic declaration, whose binders it would need applied, or
    /// it's overloaded.
    pub(super) fn lambda(&mut self, at: At, func: FuncId) -> TypeId {
        let unknown = self.db.unknown();
        let data = self.ir.func(func);
        if let Some(signature) = &data.signature {
            let Some(mut declared) = self.declared[func.index()].clone() else {
                return unknown;
            };
            if let Some(var) = signature.result {
                declared.result = self.joined(var, at);
            }
            return self.db.intern(Type::Function(declared));
        }
        let FuncKind::Decl(decl) = data.kind else {
            return unknown;
        };
        if self
            .tables
            .lifted
            .get(&decl)
            .is_some_and(|lifted| !lifted.is_empty())
        {
            return unknown;
        }
        self.function_value(decl)
    }

    /// A def's value: its type, unless it's overloaded, which isn't resolved yet
    fn function_value(&self, decl: DeclId) -> TypeId {
        match self.tables.sig_count(decl) {
            1 => self.db.declaration(decl).ty,
            _ => self.db.unknown(),
        }
    }

    /// The value of a class object: `Type[C]`, with a generic class's binders
    /// hoisted, so that `Array` is `forall T. Type[Array[T]]`
    pub(super) fn class_object(&self, decl: DeclId) -> TypeId {
        let Some(class_type) = self.db.intrinsic(Intrinsic::Type) else {
            return self.db.unknown();
        };
        let ty = self.db.declaration(decl).ty;
        let base = self.db.intern(Type::Decl(decl));
        let apply = |arg| {
            self.db.intern(Type::Apply {
                base: class_type,
                args: vec![Argument::Positional(arg)].into(),
                kind: Kind::Type,
            })
        };
        let Type::Quantified { binders, .. } = self.db.ty(ty) else {
            return apply(base);
        };
        if binders.is_empty() {
            return apply(base);
        }
        if binders
            .iter()
            .any(|binder| binder.binding != crate::typeck::r#type::Binding::Positional)
        {
            return self.db.unknown();
        }
        let args = (binders.iter().enumerate())
            .map(|(slot, binder)| {
                Argument::Positional(self.db.intern(Type::Bound {
                    reference: BoundRef::new(0, slot),
                    kind: binder.kind,
                }))
            })
            .collect();
        let instance = self.db.intern(Type::Apply {
            base,
            args,
            kind: Kind::Type,
        });
        self.db.intern(Type::Quantified {
            binders: binders.clone(),
            body: apply(instance),
        })
    }

    /// The class `C` a class object's type `Type[C]` gives
    pub(super) fn class_of(&self, mut ty: TypeId) -> Option<DeclId> {
        if let Type::Quantified { body, .. } = self.db.ty(ty) {
            ty = *body;
        }
        let class_type = self.db.intrinsic(Intrinsic::Type)?;
        match self.db.ty(ty) {
            Type::Apply { base, args, .. } if *base == class_type => match &args[..] {
                [Argument::Positional(instance)] => self.class_of_instance(*instance),
                _ => None,
            },
            _ => None,
        }
    }

    /// The class of an instance type `C` or `C[...]`
    pub(super) fn class_of_instance(&self, ty: TypeId) -> Option<DeclId> {
        let head = match self.db.ty(ty) {
            Type::Apply { base, .. } => *base,
            _ => ty,
        };
        match *self.db.ty(head) {
            Type::Decl(decl) if self.db.declaration(decl).source.kind.nominal() => Some(decl),
            _ => None,
        }
    }

    /// The value of an imported item: a def's type or a class object. A module, a
    /// module that isn't checked, and any other item are dynamic.
    fn import(&self, module: &ModuleRef, item: Option<SymbolId>) -> TypeId {
        let unknown = self.db.unknown();
        let (ModuleRef::Unit(unit), Some(item)) = (module, item) else {
            return unknown;
        };
        let mut unit: UnitId = *unit;
        let mut name: &str = self.db.symbol(item);
        // Re-exports are followed, up to a limit that only a cycle reaches
        for _ in 0..64 {
            let Some((_, target)) = self.tables.exports[unit.index()].get(name) else {
                return unknown;
            };
            match target {
                Target::Local(Referent::Decl(decl)) => return self.decl_value(*decl),
                Target::Import { module, item } => {
                    let Some(&next) = self.modules.get(module) else {
                        return unknown;
                    };
                    unit = next;
                    name = item;
                }
                _ => return unknown,
            }
        }
        unknown
    }

    fn decl_value(&self, decl: DeclId) -> TypeId {
        let declaration = self.db.declaration(decl);
        match declaration.source.kind {
            DeclKind::Class | DeclKind::Protocol => self.class_object(decl),
            DeclKind::Function => self.function_value(decl),
            DeclKind::OpaqueAlias | DeclKind::Alias | DeclKind::Closure | DeclKind::Annotation => {
                self.db.unknown()
            }
        }
    }
}
