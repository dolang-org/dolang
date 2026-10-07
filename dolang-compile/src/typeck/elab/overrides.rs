//! Overrides and protocol conformance.
//!
//! Each class and protocol must conform to each supertype it names: every public
//! member the supertype has must be matched by a compatible member of its own,
//! overriding or inherited, and a class must provide each member a protocol
//! declares. The solver states what each member requires (see
//! `solver/conform.rs`); this pass relates the requirements under the
//! declaration's rigids and reports them. A class must also inherit at runtime
//! each class that a supertype it only claims names, with compatible arguments,
//! as a protocol's supertypes are claims too.
//!
//! Every check is local: it assumes only the declaration's own bounds, and no
//! verdict is cached. Each is solved on its own fork of a solver holding the
//! declaration's rigids, with its own budget, so a class with many members
//! doesn't exhaust one budget for all of them. A check the solver can't decide is
//! returned as unresolved.

use std::collections::HashMap;

use super::{
    DeclNode, Designated, Diag, Nonconforming, Tables, Uncallable, UnitDiag, Unresolved, sig,
    surface::{Class, Member, MemberScope},
};
use crate::{
    ast::SpecialMethod,
    source::Span,
    typeck::{
        solver::{
            Inheritance, Issue, Provenance, Reach, Requirement, RequirementKind, Residual, Solver,
            Status, Term,
        },
        r#type::{
            Argument, Database, DeclId, DeclKind, Intrinsic, Kind, MemberKey, Scope, Type, TypeId,
            UnitId, UnitSpan,
        },
    },
};

/// Check that every class and protocol conforms to its supertypes. Violations
/// are diagnosed; undecided checks are returned.
pub(crate) fn overrides(
    db: &Database,
    tables: &Tables<'_>,
    diags: &mut Vec<UnitDiag>,
) -> Vec<Unresolved> {
    let mut unresolved = Vec::new();
    for (index, decl) in tables.decls.iter().enumerate() {
        let DeclNode::Class(class) = &decl.node else {
            continue;
        };
        let mut check = Check {
            db,
            tables,
            id: DeclId::from_index(index),
            unit: decl.unit,
            class,
            diags: &mut *diags,
            unresolved: &mut unresolved,
        };
        check.run();
    }
    unresolved
}

struct Check<'a, 'u> {
    db: &'a Database,
    tables: &'a Tables<'u>,
    id: DeclId,
    unit: UnitId,
    class: &'a Class,
    diags: &'a mut Vec<UnitDiag>,
    unresolved: &'a mut Vec<Unresolved>,
}

/// A check whose judgments are to be solved together and reported
struct Pending {
    span: Span,
    /// The judgments, each actual below its expected, all of which must hold
    pairs: Vec<(Term, Term)>,
    message: String,
}

impl Check<'_, '_> {
    fn run(&mut self) {
        // A type object's members are its class's, so it is called through the
        // class's `(init)` or class-level `(call)`, which the solver relates to a
        // function type directly
        let designated = self.tables.designated.get(&self.id);
        if designated == Some(&Designated::Intrinsic(Intrinsic::Type)) {
            return;
        }
        let db = self.db;
        let runtime = db.declaration(self.id).source.kind == DeclKind::Class;
        let mut solver = Solver::new(db);
        #[cfg(feature = "debug")]
        {
            let tables = self.tables;
            solver.named(move |ty| tables.render_type(db, ty));
        }
        let environment = solver.rigid_environment(self.id);
        solver.close();
        let instance = self.instance();
        self.callable(&solver, instance);
        let spans = self.member_spans();
        let mut pending = Vec::new();
        for super_ref in &self.class.supers {
            let span = super_ref.span();
            let Some(&ty) = self.tables.expr_types.get(&UnitSpan {
                unit: self.unit,
                span,
            }) else {
                continue;
            };
            let supertype = solver.view(ty, environment);
            let requirements = match solver.conformance(instance, supertype, runtime) {
                Ok(requirements) => requirements,
                Err(issue) => {
                    self.undecided(span, issue);
                    continue;
                }
            };
            for requirement in requirements {
                // A claimed class's member, as the class inherits it, agrees with
                // itself unless the class is inherited with other arguments, which
                // is reported below
                let claimed = runtime
                    && super_ref.type_only
                    && requirement.provider == Some(requirement.required)
                    && db.declaration(requirement.required).source.kind == DeclKind::Class;
                if !claimed {
                    self.requirement(&mut pending, &spans, span, requirement);
                }
            }
            // A claim doesn't make the runtime inherit what it names
            if runtime && super_ref.type_only {
                let name = &self
                    .tables
                    .dotted(self.unit, super_ref.head, &super_ref.fields);
                match solver.claimed_classes(solver.closed(instance), supertype) {
                    Ok(classes) => {
                        for (class, inheritance) in classes {
                            self.inheritance(&mut pending, span, name, class, inheritance);
                        }
                    }
                    Err(issue) => self.undecided(span, issue),
                }
            }
        }
        if designated != Some(&Designated::Value) {
            self.value(&solver, instance, runtime, &spans, &mut pending);
        }
        for check in pending {
            let mut fork = solver.clone();
            for (actual, expected) in check.pairs {
                fork.constrain(actual, expected, Provenance::default());
            }
            let outcomes = fork.solve();
            if outcomes.iter().any(|o| o.status == Status::Contradicted) {
                self.report(check.span, check.message);
            } else if let Some(outcome) = outcomes.iter().find(|o| o.status == Status::Unresolved) {
                // A form the checker doesn't support leaves what depends on it
                // unsolved, so it's the reason
                let residuals =
                    outcome
                        .diagnostics
                        .iter()
                        .filter_map(|diagnostic| match diagnostic.issue {
                            Issue::Residual(residual) => Some(residual),
                            Issue::Contradiction(_) => None,
                        });
                let residual = residuals
                    .clone()
                    .find(|residual| matches!(residual, Residual::Unsupported(_)))
                    .or_else(|| residuals.clone().next())
                    .unwrap_or(Residual::Unsupported("no reason recorded"));
                self.unresolved.push(Unresolved {
                    span: UnitSpan {
                        unit: self.unit,
                        span: check.span,
                    },
                    residual,
                });
            }
        }
    }

    /// Warn of an instance `(call)` in a class that doesn't reach `Func`, since
    /// the checker won't pass its instances as functions. A subclass may reach
    /// it, so passing one is never an error.
    fn callable(&mut self, solver: &Solver<'_>, instance: TypeId) {
        let (db, tables, unit) = (self.db, self.tables, self.unit);
        let call = self.class.members.iter().find_map(|member| {
            let Member::Method { decl, sig } = *member else {
                return None;
            };
            let method = tables.method(decl, sig);
            let instance = sig::method_scope(tables, unit, method) == Scope::Instance;
            (instance && matches!(method.special, Some(SpecialMethod::Call)))
                .then_some(method.name.span)
        });
        let Some(span) = call else {
            return;
        };
        let Some(&Type::Decl(func)) = db.intrinsic(Intrinsic::Func).map(|func| db.ty(func)) else {
            return;
        };
        if let Ok(Reach::Unreached) = solver.reach(solver.closed(instance), func) {
            let class = self.name(self.id).to_owned();
            self.diags
                .push((self.unit, Diag::new(Uncallable { span, class })));
        }
    }

    /// Relate the members the declaration declares itself to `Value`'s, which it
    /// has without naming it. One it inherits was related where it's declared.
    fn value(
        &mut self,
        solver: &Solver<'_>,
        instance: TypeId,
        runtime: bool,
        spans: &HashMap<(MemberKey, bool), Span>,
        pending: &mut Vec<Pending>,
    ) {
        let db = self.db;
        if db.intrinsic(Intrinsic::Value).is_none() {
            return;
        }
        let Some(name) = self.tables.decls[self.id.index()].name else {
            return;
        };
        match solver.conformance(instance, solver.closed(db.top()), runtime) {
            Ok(requirements) => {
                for requirement in requirements {
                    if requirement.provider == Some(self.id) {
                        self.requirement(pending, spans, name.span, requirement);
                    }
                }
            }
            Err(issue) => self.undecided(name.span, issue),
        }
    }

    /// Relate or report what a supertype's member requires
    fn requirement(
        &mut self,
        pending: &mut Vec<Pending>,
        spans: &HashMap<(MemberKey, bool), Span>,
        supertype: Span,
        requirement: Requirement,
    ) {
        let member = self.member(requirement.key);
        let required = format!("{}.{member}", self.name(requirement.required));
        // An override is reported where it's declared
        let declared = requirement.provider == Some(self.id);
        let span = match declared {
            true => spans
                .get(&(requirement.key, requirement.scope == Scope::Instance))
                .copied()
                .unwrap_or(supertype),
            false => supertype,
        };
        let subject = match (declared, requirement.provider) {
            (false, Some(provider)) => format!("`{member}` from `{}`", self.name(provider)),
            _ => format!("`{member}`"),
        };
        match requirement.kind {
            RequirementKind::Relate(pairs) => {
                pending.push(Pending {
                    span,
                    pairs,
                    message: format!("{subject} does not match `{required}`"),
                });
            }
            RequirementKind::Missing => {
                let message = format!(
                    "`{}` does not provide `{member}`, which `{}` requires",
                    self.name(self.id),
                    self.name(requirement.required)
                );
                self.report(span, message);
            }
            RequirementKind::Changed {
                provided,
                required: kind,
            } => {
                let message = format!("{subject} is {provided}, but `{required}` is {kind}");
                self.report(span, message);
            }
            RequirementKind::Undecided(issue) => self.undecided(span, issue),
        }
    }

    /// Relate or report how a class inherits a class its claim names
    fn inheritance(
        &mut self,
        pending: &mut Vec<Pending>,
        span: Span,
        claim: &str,
        class: DeclId,
        inheritance: Inheritance,
    ) {
        let own = self.name(self.id);
        let named = self.name(class);
        match inheritance {
            Inheritance::Unreached => {
                let message =
                    format!("`{claim}` requires `{named}`, which `{own}` does not inherit");
                self.report(span, message);
            }
            Inheritance::Relate(actual, expected) => {
                pending.push(Pending {
                    span,
                    pairs: vec![(actual, expected)],
                    message: format!(
                        "`{own}` inherits `{named}` with arguments `{claim}` does not allow"
                    ),
                });
            }
            Inheritance::Undecided(issue) => self.undecided(span, issue),
        }
    }

    /// The declaration's own type, its class applied to its rigids
    fn instance(&self) -> TypeId {
        let db = self.db;
        let decl = db.intern(Type::Decl(self.id));
        let rigids = db.rigids(self.id);
        if rigids.is_empty() {
            return decl;
        }
        db.intern(Type::Apply {
            base: decl,
            args: rigids
                .into_iter()
                .map(Argument::Positional)
                .collect::<Vec<_>>()
                .into(),
            kind: Kind::Type,
        })
    }

    /// Where each member the class declares is named, by its name and whether it
    /// is an instance member
    fn member_spans(&self) -> HashMap<(MemberKey, bool), Span> {
        let (db, tables, unit) = (self.db, self.tables, self.unit);
        let mut spans = HashMap::new();
        for member in &self.class.members {
            match *member {
                Member::Field(ref field) => {
                    for &name in &field.names {
                        let key = MemberKey {
                            name: db.intern_symbol(tables.name(unit, name)),
                            special: false,
                            private: !field.public,
                        };
                        spans
                            .entry((key, field.scope == MemberScope::Instance))
                            .or_insert(name.span);
                    }
                }
                Member::Method { decl, sig } => {
                    let method = tables.method(decl, sig);
                    let key = MemberKey {
                        name: db.intern_symbol(tables.name(unit, method.name)),
                        special: method.special.is_some(),
                        private: !method.public && method.special.is_none(),
                    };
                    let instance = sig::method_scope(tables, unit, method) == Scope::Instance;
                    spans.entry((key, instance)).or_insert(method.name.span);
                }
            }
        }
        spans
    }

    fn member(&self, key: MemberKey) -> String {
        let name = self.db.symbol(key.name);
        match key.special {
            true => format!("({name})"),
            false => name.to_owned(),
        }
    }

    fn name(&self, decl: DeclId) -> &str {
        self.db
            .declaration(decl)
            .source
            .name
            .map_or("", |name| self.db.symbol(name))
    }

    fn report(&mut self, span: Span, message: String) {
        self.diags
            .push((self.unit, Diag::new(Nonconforming { span, message })));
    }

    fn undecided(&mut self, span: Span, issue: Issue) {
        let residual = match issue {
            Issue::Residual(residual) => residual,
            Issue::Contradiction(_) => Residual::Unsupported("a contradicted member lookup"),
        };
        self.unresolved.push(Unresolved {
            span: UnitSpan {
                unit: self.unit,
                span,
            },
            residual,
        });
    }
}
