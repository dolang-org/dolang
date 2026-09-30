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
//! verdict is cached. A check the solver can't decide is returned as unresolved.

use std::collections::HashMap;

use super::{DeclNode, Nonconforming, Tables, UnitDiag, Unresolved, sig};
use crate::{
    ast::{Class, ClassMember, MemberScope},
    source::{self, Span},
    typeck::{
        solver::{
            ConstraintId, Inheritance, Issue, Outcome, Provenance, Requirement, RequirementKind,
            Residual, Solver, Status,
        },
        r#type::{
            Argument, Database, DeclId, DeclKind, Kind, MemberKey, Scope, Type, TypeId, UnitId,
            UnitSpan,
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
        let DeclNode::Class(class) = decl.node else {
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
    class: &'u Class,
    diags: &'a mut Vec<UnitDiag>,
    unresolved: &'a mut Vec<Unresolved>,
}

/// A check whose judgments are constrained, to be reported once solved
struct Pending {
    span: Span,
    /// The judgments' constraints, all of which must hold
    constraints: Vec<ConstraintId>,
    message: String,
}

impl Check<'_, '_> {
    fn run(&mut self) {
        let db = self.db;
        let runtime = db.declaration(self.id).source.kind == DeclKind::Class;
        let mut solver = Solver::new(db);
        let environment = solver.rigid_environment(self.id);
        solver.close();
        let instance = self.instance();
        let spans = self.member_spans();
        let mut pending = Vec::new();
        for super_ref in &self.class.super_refs {
            let head = super_ref.ident.span;
            let span = super_ref.fields.last().map_or(head, |field| head | field);
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
                    self.requirement(&mut solver, &mut pending, &spans, span, requirement);
                }
            }
            // A claim doesn't make the runtime inherit what it names
            if runtime && super_ref.type_only {
                let name = self.tables.text(self.unit, span);
                match solver.claimed_classes(solver.closed(instance), supertype) {
                    Ok(classes) => {
                        for (class, inheritance) in classes {
                            self.inheritance(
                                &mut solver,
                                &mut pending,
                                span,
                                name,
                                class,
                                inheritance,
                            );
                        }
                    }
                    Err(issue) => self.undecided(span, issue),
                }
            }
        }
        let outcomes = solver.solve();
        for check in pending {
            let outcomes: Vec<&Outcome> = outcomes
                .iter()
                .filter(|outcome| check.constraints.contains(&outcome.constraint))
                .collect();
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

    /// Relate or report what a supertype's member requires
    fn requirement(
        &mut self,
        solver: &mut Solver<'_>,
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
                let constraints = pairs
                    .into_iter()
                    .map(|(actual, expected)| {
                        solver.constrain(actual, expected, Provenance::default())
                    })
                    .collect();
                pending.push(Pending {
                    span,
                    constraints,
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
        solver: &mut Solver<'_>,
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
                let constraint = solver.constrain(actual, expected, Provenance::default());
                pending.push(Pending {
                    span,
                    constraints: vec![constraint],
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
        for member in &self.class.body.members {
            match member {
                ClassMember::Field(field) => {
                    for name in &field.fields {
                        let key = MemberKey {
                            name: db.intern_symbol(tables.text(unit, name.ident.span)),
                            special: false,
                            private: field.pub_span.is_none(),
                        };
                        spans
                            .entry((key, matches!(field.scope, MemberScope::Instance)))
                            .or_insert(name.ident.span);
                    }
                }
                ClassMember::Method(method) => {
                    let key = MemberKey {
                        name: db.intern_symbol(tables.text(unit, method.name_span)),
                        special: method.special.is_some(),
                        private: method.pub_span.is_none() && method.special.is_none(),
                    };
                    let instance = sig::method_scope(tables, unit, method) == Scope::Instance;
                    spans.entry((key, instance)).or_insert(method.name_span);
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
        self.diags.push((
            self.unit,
            source::Diag::new(Nonconforming { span, message }),
        ));
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
