//! Expressions: their types in a state, and the class and import values they name.

use std::collections::VecDeque;

use crate::source::Span;

use super::{At, Flow, State, problem::Problem, rule::passed_signature};
use crate::typeck::{
    cfg::{Expr, ExprKind, FuncId, FuncKind},
    elab::{Designated, ModuleRef, Referent, Tables, Target},
    solver::Status,
    r#type::{
        Argument, Binding, BoundRef, Database, DeclId, DeclKind, Intrinsic, Kind, SymbolId, Type,
        TypeId, UnitId, UnitSpan,
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
            ExprKind::Fmt(parts) => self.fmt_seq(at, state, operands, parts, expected),
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
            ExprKind::TypeTest { value, .. } => {
                self.eval(at, state, operands, value);
                self.intrinsic(Intrinsic::Bool)
            }
            &ExprKind::Cast {
                ref value,
                ty,
                checked,
            } => self.cast(at, state, operands, value, ty, checked),
            &ExprKind::Class(decl) => class_object(self.db, decl),
            ExprKind::Import { module, item } => self.import(module, *item),
            &ExprKind::Lambda(func) => self.expected_lambda(at, func, expected, expr.span),
            ExprKind::Call { .. } => self.call(at, state, operands, expr, expected),
            ExprKind::Invoke { .. } => self.invoke(at, state, operands, expr, expected),
            ExprKind::Get { .. } => self.get(at, state, operands, expr, expected),
            ExprKind::Index { .. } => self.index(at, state, operands, expr, expected),
            ExprKind::Unary { .. } => self.unary(at, state, operands, expr),
            ExprKind::Binary { .. } => self.binary(at, state, operands, expr),
            ExprKind::Range { .. } => self.range(at, state, operands, expr, expected),
            ExprKind::Collection { .. } => self.collection(at, state, operands, expr, expected),
            ExprKind::Operand => operands.pop_front().expect("an operand for each hole"),
            ExprKind::Never => self.db.bottom(),
            ExprKind::Namespace | ExprKind::Error => unknown,
        }
    }

    /// A cast's type, unless its value never arrives. A checked cast gives its value
    /// the type as an expectation, and reports a value that doesn't fit it. When
    /// reporting, an unchecked cast first tries the check, and warns if it passes.
    fn cast(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        value: &Expr,
        ty: TypeId,
        checked: bool,
    ) -> TypeId {
        let span = value.span;
        if !checked
            && self.observing()
            && span != Span::INVALID
            && self.trial(at, state, operands, value, ty)
        {
            self.problem(Problem::Assertion {
                span,
                ty: self.subject(ty),
            });
        }
        let found = match checked {
            true => self.expect(at, state, operands, value, Some(ty)),
            false => self.eval(at, state, operands, value),
        };
        if found == self.db.bottom() {
            return found;
        }
        if checked
            && self.observing()
            && span != Span::INVALID
            && self.conform(found, ty, span) == Status::Contradicted
        {
            let (found, ty) = self.pair(found, ty);
            self.problem(Problem::Cast { span, found, ty });
        }
        ty
    }

    /// Whether a value can be shown to fit a type it's expected of, on copies of
    /// its operands and state, leaving nothing the trial reports or records
    fn trial(
        &mut self,
        at: At,
        state: &State,
        operands: &VecDeque<TypeId>,
        value: &Expr,
        ty: TypeId,
    ) -> bool {
        let saved = self.results.clone();
        let mut operands = operands.iter().take(super::holes(value)).copied().collect();
        let found = self.expect(at, &mut state.clone(), &mut operands, value, Some(ty));
        let fits = found != self.db.bottom() && self.relate(found, ty).status == Status::Proven;
        self.results = saved;
        fits
    }

    /// Instantiate a closure outside a rule that types it. Nothing here gives a
    /// `do` block's parameters and channels anything; once the analysis is stuck,
    /// those still bottom are dynamic (see [`Flow::analyze`]). Its value is
    /// its declared type under its rigids, with the result its variable joins, if
    /// that's left to it, and in a strict unit, so are its parameters and
    /// channels. A nested def's value is its declared type, unless it's
    /// nested in a generic declaration, whose binders it would need applied, or
    /// it's overloaded. What it can't type is a gap at `span`.
    pub(super) fn lambda(&mut self, at: At, func: FuncId, span: Span) -> TypeId {
        let unknown = self.db.unknown();
        let data = self.ir.func(func);
        if let Some(signature) = &data.signature {
            let Some(mut declared) = self.declared[func.index()].clone() else {
                return self.gap(span, "a `do` block without a declared type");
            };
            // In a strict unit, what it's passed is what its body was checked with
            if self.strict()
                && let Some(passed) =
                    passed_signature(self.db, data, &declared, |var| self.joined(var, at))
            {
                declared = passed;
            }
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
            return self.gap(span, "a nested def's value in a generic declaration");
        }
        function_value(self.db, self.tables, decl)
    }

    /// Whether a value is an overloaded function: an overloaded def, or a
    /// method's overloads
    pub(super) fn overloaded(&self, ty: TypeId) -> bool {
        match *self.db.ty(ty) {
            Type::Overloaded { .. } => true,
            Type::Decl(decl) => {
                self.db.declaration(decl).source.kind == DeclKind::Function
                    && !self.db.overloads(decl).is_empty()
            }
            _ => false,
        }
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

    /// The value of an imported item: a def's type, a class object, or a variable's
    /// annotation. A module, a module that isn't checked, an unannotated variable,
    /// and any other item are dynamic.
    fn import(&self, module: &ModuleRef, item: Option<SymbolId>) -> TypeId {
        let Some(item) = item else {
            return self.db.unknown();
        };
        match self.export(module, item) {
            Some(Target::Local(Referent::Decl(decl))) => self.decl_value(*decl),
            Some(Target::Local(Referent::Value(value))) => self.variable(value),
            _ => self.db.unknown(),
        }
    }

    /// What writing a module's member must give: what reading it gives. A module
    /// it re-exports can't be written, since typing names it by import.
    pub(super) fn written_import(&self, module: &ModuleRef, item: SymbolId) -> Option<TypeId> {
        match self.export(module, item) {
            Some(Target::Module(_)) => None,
            _ => Some(self.import(module, Some(item))),
        }
    }

    /// The export a checked module's item names, following re-exports of items
    fn export(&self, module: &ModuleRef, item: SymbolId) -> Option<&Target<'_>> {
        let ModuleRef::Unit(unit) = module else {
            return None;
        };
        let mut unit: UnitId = *unit;
        let mut name: &str = self.db.symbol(item);
        // Re-exports are followed, up to a limit that only a cycle reaches
        for _ in 0..64 {
            let (_, target) = self.tables.exports[unit.index()].get(name)?;
            match target {
                Target::Import { module, item } => {
                    unit = *self.modules.get(module)?;
                    name = item;
                }
                target => return Some(target),
            }
        }
        None
    }

    /// An exported variable's annotation, written at the top level, so with no
    /// binders to stand rigids in for
    fn variable(&self, value: &UnitSpan) -> TypeId {
        match self.tables.values.get(value) {
            Some(&Some(site)) => {
                let site = &self.tables.sites[site.index()];
                self.tables.site_types[&UnitSpan {
                    unit: site.unit,
                    span: site.ty.span(),
                }]
            }
            _ => self.db.unknown(),
        }
    }

    fn decl_value(&self, decl: DeclId) -> TypeId {
        let declaration = self.db.declaration(decl);
        match declaration.source.kind {
            DeclKind::Class | DeclKind::Protocol => class_object(self.db, decl),
            DeclKind::Function => function_value(self.db, self.tables, decl),
            DeclKind::OpaqueAlias | DeclKind::Alias | DeclKind::Closure | DeclKind::Annotation => {
                self.db.unknown()
            }
        }
    }
}

/// A def's value: its type. An overloaded def's is the def itself, which the
/// solver relates as its overloads (see [`Type::Overloaded`]). Without an
/// implementation, it's dynamic.
pub(crate) fn function_value(db: &Database, tables: &Tables<'_>, decl: DeclId) -> TypeId {
    match tables.sig_count(decl) {
        1 => db.declaration(decl).ty,
        _ => match db.implementation(decl) {
            Some(_) => db.intern(Type::Decl(decl)),
            None => db.unknown(),
        },
    }
}

/// The value of a class object: `Type[C]`, with a generic class's binders
/// hoisted, so that `Array` is `forall T. Type[Array[T]]`
pub(crate) fn class_object(db: &Database, decl: DeclId) -> TypeId {
    let Some(class_type) = db.intrinsic(Intrinsic::Type) else {
        return db.unknown();
    };
    let ty = db.declaration(decl).ty;
    let base = db.intern(Type::Decl(decl));
    let apply = |arg| {
        db.intern(Type::Apply {
            base: class_type,
            args: vec![Argument::Positional(arg)].into(),
            kind: Kind::Type,
        })
    };
    let Type::Quantified { binders, .. } = db.ty(ty) else {
        return apply(base);
    };
    if binders.is_empty() {
        return apply(base);
    }
    // An application gives a keyword binder's argument in its slot, as a
    // positional binder's
    if binders
        .iter()
        .any(|binder| !matches!(binder.binding, Binding::Positional | Binding::Keyword(_)))
    {
        return db.unknown();
    }
    let args = (binders.iter().enumerate())
        .map(|(slot, binder)| {
            Argument::Positional(db.intern(Type::Bound {
                reference: BoundRef::new(0, slot),
                kind: binder.kind,
            }))
        })
        .collect();
    let instance = db.intern(Type::Apply {
        base,
        args,
        kind: Kind::Type,
    });
    db.intern(Type::Quantified {
        binders: binders.clone(),
        body: apply(instance),
    })
}
