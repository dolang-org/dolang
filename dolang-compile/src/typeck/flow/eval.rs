//! Expressions: their types in a state, and the class and import values they name.

use std::collections::VecDeque;

use super::{At, Flow, State};
use crate::typeck::{
    cfg::{Expr, ExprKind, FmtSpec, FuncId, FuncKind, Item},
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
            ExprKind::BinConcat { parts, .. } => {
                for part in parts {
                    self.eval(at, state, operands, part);
                }
                self.designated_type(Designated::Bin)
            }
            ExprKind::Fmt(parts) => {
                for part in parts {
                    self.eval(at, state, operands, part);
                }
                unknown
            }
            ExprKind::FmtValue { value, spec, .. } => {
                self.eval(at, state, operands, value);
                self.spec(at, state, operands, spec);
                unknown
            }
            ExprKind::FmtParam { spec, .. } => {
                self.spec(at, state, operands, spec);
                unknown
            }
            &ExprKind::Var(var) => {
                let fact = self.read(at, state, var);
                self.record(expr.span, fact);
                fact.ty
            }
            &ExprKind::Class(decl) => self.class_object(decl),
            ExprKind::Import { module, item } => self.import(module, *item),
            &ExprKind::Lambda(func) => self.lambda(func),
            ExprKind::Call { callee, args, .. } => {
                self.eval(at, state, operands, callee);
                self.items(at, state, operands, args);
                unknown
            }
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
            ExprKind::Collection { items, .. } => {
                self.items(at, state, operands, items);
                unknown
            }
            ExprKind::Operand => operands.pop_front().expect("an operand for each hole"),
            ExprKind::Never => self.db.bottom(),
            ExprKind::AmbientInput | ExprKind::Namespace | ExprKind::Error => unknown,
        }
    }

    fn spec(&mut self, at: At, state: &mut State, operands: &mut VecDeque<TypeId>, spec: &FmtSpec) {
        for part in [&spec.width, &spec.precision].into_iter().flatten() {
            self.eval(at, state, operands, part);
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
                Item::For(items) => self.items(at, state, operands, items),
                Item::If { then, else_ } => {
                    self.items(at, state, operands, then);
                    self.items(at, state, operands, else_);
                }
            }
        }
    }

    /// Instantiate a closure. Nothing is expected of a `do` block's parameters and
    /// channels here, so they're dynamic. Its value is its declared type, unless it's
    /// nested in a generic declaration, whose binders it would need applied.
    fn lambda(&mut self, func: FuncId) -> TypeId {
        let unknown = self.db.unknown();
        let data = self.ir.func(func);
        if let Some(signature) = &data.signature {
            let expected = (signature.params.iter().copied())
                .chain([signature.input, signature.output])
                .flatten();
            for var in expected {
                self.join(var, unknown);
            }
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
        self.db.declaration(decl).ty
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
            DeclKind::Function => declaration.ty,
            DeclKind::OpaqueAlias | DeclKind::Alias | DeclKind::Closure | DeclKind::Annotation => {
                self.db.unknown()
            }
        }
    }
}
