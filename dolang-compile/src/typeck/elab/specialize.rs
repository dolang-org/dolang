//! Receiver specialization, after the database is sealed.
//!
//! An instance method's receiver annotated `self @ U` must reach the method's
//! class through the substitution-carrying ancestor walk, run under the method's
//! rigids. An `@def` overload is specialized: the arguments its receiver reaches
//! the class with replace the class's binders throughout its type. `self` keeps
//! `U` verbatim. The overload stays lifted over every class binder; a replaced
//! one is simply unused. An implementation may not specialize: its receiver must
//! reach the class with the class's own binders. A receiver that doesn't reach
//! its class, or an implementation's that specializes it, keeps the
//! unspecialized type and is diagnosed.
//!
//! Only class supertypes and a method's own binder bounds are read, so the order
//! in which methods are specialized doesn't matter.

use super::{BadReceiver, DeclNode, Diag, ParamTy, Slot, SpecializedReceiver, Tables, UnitDiag};
use crate::typeck::{
    solver::{Reach, Solver},
    r#type::{Binder, Database, DeclId, Element, Type, TypeId},
};

/// Specialize every method with an annotated receiver.
pub(crate) fn specialize(db: &mut Database, tables: &Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut rewrites = Vec::new();
    for (index, decl) in tables.decls.iter().enumerate() {
        let DeclNode::Methods(methods) = &decl.node else {
            continue;
        };
        let id = DeclId::from_index(index);
        let (class_id, _) = decl.outer.expect("a method is declared in a class");
        for (sig, written) in methods.iter().enumerate() {
            let completed = &tables.sigs[&(id, sig)];
            let Some(&ParamTy::Single(Slot::Annot(annot))) =
                completed.params.first().filter(|_| completed.receiver)
            else {
                continue;
            };
            let method = tables.sig_decls[&(id, sig)];
            let Some(receiver) = receiver(db, method) else {
                continue;
            };
            let group = &tables.groups[&(id, sig)];
            let slots: Vec<usize> = tables.groups[&(class_id, 0)]
                .iter()
                .map(|binder| {
                    group
                        .iter()
                        .position(|b| b == binder)
                        .expect("a method is lifted over its class's binders")
                })
                .collect();
            let span = tables.site_ty(annot).span();
            let class = || {
                let owner = &tables.decls[class_id.index()];
                let name = owner.name.map_or("", |name| tables.name(owner.unit, name));
                name.to_owned()
            };
            let bad = |undecided| {
                Diag::new(BadReceiver {
                    span,
                    class: class(),
                    undecided,
                })
            };
            let diag = match walk(db, method, receiver, class_id) {
                Walk::Reached(args) if written.overload => {
                    rewrites.push((method, slots, args));
                    continue;
                }
                Walk::Reached(args) if identity(db, method, &slots, &args) => continue,
                Walk::Reached(_) => Diag::new(SpecializedReceiver {
                    span,
                    class: class(),
                }),
                Walk::Dynamic => continue,
                Walk::Unreached => bad(false),
                Walk::Undecided => bad(true),
            };
            diags.push((decl.unit, diag));
        }
    }
    for (method, slots, args) in rewrites {
        rewrite(db, method, &slots, args);
    }
}

/// The type a method's receiver is annotated with, in its group
fn receiver(db: &Database, method: DeclId) -> Option<TypeId> {
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
    let mut solver = Solver::new(db);
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
    let ty = db.intern(Type::Quantified {
        binders: binders.into(),
        body: replace(*body),
    });
    db.retype(method, ty);
}
