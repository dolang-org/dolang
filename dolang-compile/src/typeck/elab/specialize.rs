//! Receiver checks and specialization, after the database is sealed.
//!
//! An instance method's receiver may be annotated `self @ U`, and the two kinds
//! of signature relate `U` to the class `C` in opposite directions:
//!
//! - An `@def` overload narrows what callers may pass, so `U` must reach `C`
//!   through the substitution-carrying ancestor walk, run under the method's
//!   rigids. The overload is specialized: the arguments its receiver reaches the
//!   class with replace the class's binders throughout its type. `self` keeps
//!   `U` verbatim. The overload stays lifted over every class binder; a replaced
//!   one is simply unused.
//!   An overload whose receiver doesn't reach `C` may still be below its
//!   implementation's receiver `W`, as `BaseIterable[Str]` is below
//!   `BaseIterable[T]`. It's specialized by what the class's binders are solved
//!   as for `U <: W`, and a binder that isn't solved stays as it is.
//! - An implementation belongs to every instance, so `C` applied to its own
//!   binders must be below `U` for some choice of the method's own binders:
//!   the class itself or anything wider. `[X @ R] self @ X` holds when `C` is
//!   below `R`, and `[V] self @ BaseIterable[V]` when the class is iterable,
//!   since each call chooses those binders from its receiver. Nothing is
//!   specialized.
//!
//! A receiver that fails its check keeps the unspecialized type and is
//! diagnosed. Only class supertypes and a method's own binder bounds are read,
//! so the order in which methods are specialized doesn't matter.

use super::{
    BadReceiver, DeclNode, Diag, ParamTy, ReceiverBound, SiteId, Slot, SpecializedReceiver, Tables,
    UnitDiag,
};
use crate::typeck::{
    solver::{Gas, Provenance, Reach, Solver, Status, Term},
    r#type::{
        Argument, Binder, Binding, Database, DeclId, Element, Function, Kind, Rest, Scope, Type,
        TypeId,
    },
};

/// Specialize every method with an annotated receiver.
pub(crate) fn specialize(db: &mut Database, tables: &Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut rewrites = Vec::new();
    for id in tables.decl_ids() {
        let decl = tables.decl(id);
        let DeclNode::Methods(methods) = &decl.node else {
            continue;
        };
        let (class_id, _) = decl.outer.expect("a method is declared in a class");
        for (sig, written) in methods.iter().enumerate() {
            let Some(annot) = annotated(tables, (id, sig)) else {
                continue;
            };
            let method = tables
                .sig_decl((id, sig))
                .expect("every signature is declared");
            let Some(receiver) = receiver_of(db, method) else {
                continue;
            };
            let slots = class_slots(tables, (id, sig), class_id);
            let span = tables.site_ty(annot).span();
            let class = || {
                let owner = tables.decl(class_id);
                let name = owner.name.map_or("", |name| tables.name(owner.unit, name));
                name.to_owned()
            };
            let overload = written.overload;
            let bad = |bound, undecided| {
                Diag::new(BadReceiver {
                    span,
                    class: class(),
                    bound,
                    undecided,
                })
            };
            let walked = walk(db, method, receiver, class_id);
            let diag = match walked {
                Walk::Reached(args) if overload => {
                    rewrites.push((method, slots, args));
                    continue;
                }
                Walk::Dynamic => continue,
                Walk::Unreached | Walk::Undecided if overload => {
                    // An implementation whose receiver isn't annotated takes the
                    // class, which the walk didn't reach
                    let implementation = methods
                        .iter()
                        .position(|written| !written.overload)
                        .filter(|&sig| annotated(tables, (id, sig)).is_some())
                        .map(|sig| {
                            let decl = tables
                                .sig_decl((id, sig))
                                .expect("every signature is declared");
                            (decl, class_slots(tables, (id, sig), class_id))
                        });
                    let fits = implementation.and_then(|(decl, own)| {
                        let wide = receiver_of(db, decl)?;
                        Some(below(db, method, receiver, decl, wide, &own))
                    });
                    match fits {
                        Some(Ok(args)) => {
                            let args = args
                                .into_iter()
                                .zip(&slots)
                                .map(|(arg, &slot)| arg.unwrap_or(db.rigids(method)[slot]))
                                .collect();
                            rewrites.push((method, slots, args));
                            continue;
                        }
                        Some(Err(status)) => {
                            bad(ReceiverBound::Implementation, status == Status::Unresolved)
                        }
                        None => bad(ReceiverBound::Class, matches!(walked, Walk::Undecided)),
                    }
                }
                _ => match widens(db, method, receiver, class_id, &slots) {
                    Status::Proven => continue,
                    Status::Unresolved => bad(ReceiverBound::Instances, true),
                    Status::Contradicted if matches!(walked, Walk::Reached(_)) => {
                        Diag::new(SpecializedReceiver {
                            span,
                            class: class(),
                        })
                    }
                    Status::Contradicted => bad(ReceiverBound::Instances, false),
                },
            };
            diags.push((decl.unit, diag));
        }
    }
    for (method, slots, args) in rewrites {
        rewrite(db, method, &slots, args);
    }
}

/// The type a method's receiver is annotated with, in its group
fn receiver_of(db: &Database, method: DeclId) -> Option<TypeId> {
    let Type::Quantified { body, .. } = db.ty(db.declaration(method).ty) else {
        return None;
    };
    let Type::Function(function) = db.ty(*body) else {
        return None;
    };
    let Type::Schema(items) = db.ty(function.params) else {
        return None;
    };
    match items.first()?.element {
        Element::Positional(ty) => Some(ty),
        _ => None,
    }
}

/// Where a receiver's walk to its class ends
enum Walk {
    /// The class's arguments, closed over the method's rigids
    Reached(Vec<TypeId>),
    /// A dynamic receiver, which was already diagnosed
    Dynamic,
    Unreached,
    Undecided,
}

fn walk(db: &Database, method: DeclId, receiver: TypeId, class: DeclId) -> Walk {
    let gas = Gas::default();
    let mut solver = Solver::new(db, &gas);
    let environment = solver.rigid_environment(method);
    match solver.reach(solver.view(receiver, environment), class) {
        Ok(Reach::Reached(args)) => args
            .into_iter()
            .map(|arg| solver.reify(arg))
            .collect::<Result<_, _>>()
            .map_or(Walk::Undecided, Walk::Reached),
        Ok(Reach::Dynamic) => Walk::Dynamic,
        Ok(Reach::Unreached) => Walk::Unreached,
        Err(_) => Walk::Undecided,
    }
}

/// The annotation on an instance method signature's receiver, if it has one
fn annotated(tables: &Tables<'_>, sig: (DeclId, usize)) -> Option<SiteId> {
    let completed = tables
        .sig(sig.0, sig.1)
        .expect("every def and method has a signature");
    match completed.params.first() {
        Some(&ParamTy::Single(Slot::Annot(annot)))
            if completed.receiver == Some(Scope::Instance) =>
        {
            Some(annot)
        }
        _ => None,
    }
}

/// The slots of a signature's group that its class's binders are lifted to, in
/// the class's order
fn class_slots(tables: &Tables<'_>, sig: (DeclId, usize), class: DeclId) -> Vec<usize> {
    let group = tables.group(sig).expect("population groups each signature");
    tables
        .group((class, 0))
        .expect("population groups each signature")
        .iter()
        .map(|binder| {
            group
                .iter()
                .position(|b| b == binder)
                .expect("a method is lifted over its class's binders")
        })
        .collect()
}

/// Whether an implementation's receiver annotated `receiver`, in its group,
/// takes every instance of `class` for some choice of the method's own binders:
/// the class applied to its binders, at `slots` of the method's group, is
/// below it
fn widens(
    db: &Database,
    method: DeclId,
    receiver: TypeId,
    class: DeclId,
    slots: &[usize],
) -> Status {
    let gas = Gas::default();
    let mut solver = Solver::new(db, &gas);
    solver.rigid_environment(method);
    let rigids = db.rigids(method);
    let instance = db.intern(Type::Decl(class));
    let instance = match slots.is_empty() {
        true => instance,
        false => db.intern(Type::Apply {
            base: instance,
            args: slots
                .iter()
                .map(|&slot| Argument::Positional(rigids[slot]))
                .collect::<Vec<_>>()
                .into(),
            kind: Kind::Type,
        }),
    };
    let fixed: Vec<_> = slots
        .iter()
        .map(|&slot| (slot, solver.closed(rigids[slot])))
        .collect();
    let actual = solver.closed(instance);
    match solve_receiver(db, &mut solver, method, receiver, actual, &fixed, &[]) {
        Ok(_) => Status::Proven,
        Err(status) => status,
    }
}

/// Whether an overload's receiver annotated `receiver`, in its group, is below
/// its implementation's receiver `wide`, in the group of `implementation`, for
/// some choice of the implementation's binders. Each of the class's binders, at
/// `slots` of the implementation's group, gives what it's solved as, if it is.
fn below(
    db: &Database,
    method: DeclId,
    receiver: TypeId,
    implementation: DeclId,
    wide: TypeId,
    slots: &[usize],
) -> Result<Vec<Option<TypeId>>, Status> {
    let gas = Gas::default();
    let mut solver = Solver::new(db, &gas);
    let environment = solver.rigid_environment(method);
    let actual = solver.view(receiver, environment);
    solve_receiver(db, &mut solver, implementation, wide, actual, &[], slots)
}

/// Solve `actual <: receiver`, with `receiver` viewed in the group of `method`:
/// each slot `fixed` gives is that term, and each other is a variable below its
/// binder's bound. What each slot in `solved` is solved as, if it is: the join
/// of what's found below it, which is kept if it fits.
fn solve_receiver(
    db: &Database,
    solver: &mut Solver<'_>,
    method: DeclId,
    receiver: TypeId,
    actual: Term,
    fixed: &[(usize, Term)],
    solved: &[usize],
) -> Result<Vec<Option<TypeId>>, Status> {
    let Type::Quantified { binders, .. } = db.ty(db.declaration(method).ty) else {
        unreachable!("a method is quantified over its channels")
    };
    let group: Vec<Term> = binders
        .iter()
        .enumerate()
        .map(
            |(slot, binder)| match fixed.iter().find(|&&(at, _)| at == slot) {
                Some(&(_, term)) => term,
                None => {
                    let lanes = match binder.binding {
                        Binding::Rest(lanes) => lanes,
                        _ => Rest::All,
                    };
                    solver.infer_kind(binder.kind, lanes)
                }
            },
        )
        .collect();
    let environment = solver.environment(solver.empty_environment(), group.clone());
    let expected = solver.view(receiver, environment);
    solver.constrain(actual, expected, Provenance::default());
    for (binder, &term) in binders.iter().zip(&group) {
        if let (Some(bound), Term::Infer(_)) = (binder.bound, term) {
            let bound = solver.view(bound, environment);
            solver.constrain(term, bound, Provenance::default());
        }
    }
    let mut outcomes = solver.solve();
    // The class's binders first, so that they're what's found below them
    // rather than what another variable's default leaves them
    let mut results = Vec::new();
    for &slot in solved {
        let result = match group[slot] {
            Term::Infer(id) => solver.default_with(id, false).ok(),
            term => solver.reify(term).ok(),
        };
        results.push(result);
    }
    if !solved.is_empty() {
        outcomes = solver.solve();
    }
    // Any other binder is what's found below it, and one the receiver doesn't
    // constrain may be its bound
    loop {
        let unsolved: Vec<_> = solver.unresolved().collect();
        let progress = unsolved
            .into_iter()
            .filter(|&id| {
                solver.default_with(id, false).is_ok() || solver.default_upper(id).is_ok()
            })
            .count();
        if progress == 0 {
            break;
        }
        outcomes = solver.solve();
    }
    let status = |wanted| outcomes.iter().any(|outcome| outcome.status == wanted);
    if status(Status::Contradicted) {
        Err(Status::Contradicted)
    } else if status(Status::Unresolved) {
        Err(Status::Unresolved)
    } else {
        Ok(results)
    }
}

/// Whether `args` are the class binders at `slots` of a method's group, so the
/// receiver specializes nothing
fn identity(db: &Database, method: DeclId, slots: &[usize], args: &[TypeId]) -> bool {
    let rigids = db.rigids(method);
    slots
        .iter()
        .zip(args)
        .all(|(&slot, &arg)| rigids[slot] == arg)
}

/// Replace the class binders at `slots` of a method's group with `args`
fn rewrite(db: &mut Database, method: DeclId, slots: &[usize], args: Vec<TypeId>) {
    if identity(db, method, slots, &args) {
        return;
    }
    let mut replacements = db.rigids(method);
    for (&slot, arg) in slots.iter().zip(args) {
        replacements[slot] = arg;
    }
    let Type::Quantified { binders, body } = db.ty(db.declaration(method).ty) else {
        unreachable!()
    };
    let replace = |ty| {
        db.abstract_rigids(db.substitute(ty, &replacements), method)
            .expect("only the method's own rigids are in scope")
    };
    let binders: Vec<_> = binders
        .iter()
        .map(|binder| Binder {
            bound: binder.bound.map(replace),
            default: binder.default.map(replace),
            ..binder.clone()
        })
        .collect();
    let receiver = receiver_of(db, method).expect("a specialized method has a receiver");
    let body = replace(*body);
    let body = with_receiver(db, body, receiver);
    let ty = db.intern(Type::Quantified {
        binders: binders.into(),
        body,
    });
    db.retype(method, ty);
}

/// A method's function type with its receiver replaced by `receiver`
fn with_receiver(db: &mut Database, body: TypeId, receiver: TypeId) -> TypeId {
    let Type::Function(function) = db.ty(body) else {
        unreachable!()
    };
    let Type::Schema(items) = db.ty(function.params) else {
        unreachable!()
    };
    let mut items = items.to_vec();
    items[0].element = Element::Positional(receiver);
    let function = Function {
        params: db.intern(Type::Schema(items.into())),
        ..function.clone()
    };
    db.intern(Type::Function(function))
}
