//! Variance: which way each binder may vary while its declaration remains a subtype,
//! from how the declaration uses it.
//!
//! A def or method uses its parameters, rest parameters and ambient channels
//! contravariantly and its return type covariantly; an instance method's receiver
//! does not count. A class or protocol uses its supertypes covariantly, its public
//! fields invariantly since they are mutable, and each binder as its public and
//! special methods use it. Private members are reached only through `self`, so
//! whatever they store or return passes through a method that counts, and `(init)`
//! runs only on an object being constructed; neither counts. A field of type
//! `Phantom[...]` always counts, whatever its visibility, since it only marks its
//! arguments as used, covariantly. That privacy and `(init)` hold is elab's to
//! enforce.
//!
//! A transparent alias uses its body covariantly. A binder used in a bound of its
//! own group is invariant, and an outer binder used in the bound of a nested group
//! is used contravariantly there. Defaults, bodies and closures do not count.
//!
//! A type argument is used as the binder it fills varies, and a type declared
//! within a generic declaration takes the binders it captures as implicit
//! arguments. Those equations are solved for their least solution, which does not
//! depend on the order of declarations or units. A binder with no use, including
//! one used only through itself, is invariant, and so is a use through it.

use std::{collections::HashMap, hash::Hash};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, ParamTy, Referent, RestSlot, Slot, Tables, sig,
};
use crate::{
    ast::{
        AliasBody, ClassMember, SpecialMethod, TypeArg, TypeExpr, TypeKey, TypeParam,
        TypeParamKind, implicits,
    },
    source::Span,
    typeck::{
        elab::Fill,
        r#type::{DeclId, DeclKind, Intrinsic, UnitId, UnitSpan, Variance},
    },
};

/// Infer the variance of every binder, and of every outer binder a nested declaration
/// uses.
pub(crate) fn variances(tables: &mut Tables<'_>) {
    let mut collect = Collect {
        tables: &*tables,
        unit: UnitId::from_index(0),
        decl: DeclId::from_index(0),
        bound: None,
        path: Vec::new(),
        expanding: Vec::new(),
        saturated: false,
        constraints: Vec::new(),
    };
    let mut own = Vec::new();
    for index in 0..tables.decls.len() {
        let decl = DeclId::from_index(index);
        collect.decl(decl);
        for sig in 0..tables.sig_count(decl) {
            for slot in 0..tables.binders(decl, sig).len() {
                own.push((decl, BinderRef { decl, sig, slot }));
            }
            if let Some(completed) = tables.sigs.get(&(decl, sig)) {
                for ambient in [completed.input, completed.output] {
                    if let Ambient::Implicit(binder) = ambient {
                        own.push((decl, binder));
                    }
                }
            }
        }
    }
    let constraints = collect.constraints;

    let uses = solve(&constraints, &own);
    for &(decl, binder) in &own {
        let variance = uses
            .get(&(decl, binder))
            .map_or(Variance::Invariant, |u| u.variance());
        tables.variance.insert(binder, variance);
    }
    for (&(decl, binder), u) in &uses {
        if decl != binder.decl && *u != Use::NONE {
            tables.captured.insert((decl, binder), u.variance());
        }
    }
}

/// The directions a binder is used in so far
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Use(u8);

impl Use {
    const NONE: Use = Use(0);
    const CO: Use = Use(1);
    const CONTRA: Use = Use(2);
    const BOTH: Use = Use(3);

    fn join(self, other: Use) -> Use {
        Use(self.0 | other.0)
    }

    fn flip(self) -> Use {
        Use((self.0 & 1) << 1 | self.0 >> 1)
    }

    /// A use of `inner` in a position used as `self`
    fn then(self, inner: Use) -> Use {
        match (self, inner) {
            (Use::NONE, _) | (_, Use::NONE) => Use::NONE,
            (Use::CO, inner) => inner,
            (Use::CONTRA, inner) => inner.flip(),
            _ => Use::BOTH,
        }
    }

    fn variance(self) -> Variance {
        match self {
            Use::CO => Variance::Covariant,
            Use::CONTRA => Variance::Contravariant,
            _ => Variance::Invariant,
        }
    }
}

/// A lower bound on the use of a key
#[derive(Clone, Debug)]
enum Source<K> {
    /// A use, as the variance of each key in turn composes it
    Path(Use, Vec<K>),
    /// Every use of another key
    Join(K),
}

/// Solve for the least use of each key. A key of `own` still unused is then
/// invariant, as is a path through any unused key, so every change in the second
/// round makes a key invariant and the solution is unique.
fn solve<K: Copy + Eq + Hash>(constraints: &[(K, Source<K>)], own: &[K]) -> HashMap<K, Use> {
    let mut dependents: HashMap<K, Vec<usize>> = HashMap::new();
    for (index, (_, source)) in constraints.iter().enumerate() {
        match source {
            Source::Path(_, keys) => {
                for key in keys {
                    dependents.entry(*key).or_default().push(index);
                }
            }
            Source::Join(key) => dependents.entry(*key).or_default().push(index),
        }
    }

    let mut uses: HashMap<K, Use> = HashMap::new();
    let run = |uses: &mut HashMap<K, Use>, mut pending: Vec<usize>, unused_invariant: bool| {
        while let Some(index) = pending.pop() {
            let (target, source) = &constraints[index];
            let read = |key: &K| {
                let u = uses.get(key).copied().unwrap_or_default();
                match u {
                    Use::NONE if unused_invariant => Use::BOTH,
                    u => u,
                }
            };
            let value = match source {
                Source::Path(first, keys) => keys.iter().fold(*first, |u, key| u.then(read(key))),
                Source::Join(key) => uses.get(key).copied().unwrap_or_default(),
            };
            let entry = uses.entry(*target).or_default();
            let joined = entry.join(value);
            if joined != *entry {
                *entry = joined;
                if let Some(dependents) = dependents.get(target) {
                    pending.extend(dependents);
                }
            }
        }
    };
    run(&mut uses, (0..constraints.len()).rev().collect(), false);
    for key in own {
        uses.entry(*key).or_insert(Use::NONE);
        let u = uses.get_mut(key).expect("just inserted");
        if *u == Use::NONE {
            *u = Use::BOTH;
        }
    }
    run(&mut uses, (0..constraints.len()).rev().collect(), true);
    uses
}

/// A binder, as a declaration uses it
type Key = (DeclId, BinderRef);

struct Collect<'t, 'u> {
    tables: &'t Tables<'u>,
    unit: UnitId,
    /// The declaration whose uses are collected
    decl: DeclId,
    /// Within a bound of the binders of a signature, which are invariant there
    bound: Option<(DeclId, usize)>,
    /// The keys whose variance composes a use at the current position
    path: Vec<Key>,
    /// The written ambient channels being walked in place of a function type's own
    expanding: Vec<(DeclId, usize, usize)>,
    /// Within a channel reached from within itself, where every use is invariant
    saturated: bool,
    constraints: Vec<(Key, Source<Key>)>,
}

impl<'t, 'u> Collect<'t, 'u> {
    fn decl(&mut self, id: DeclId) {
        let tables = self.tables;
        let decl = &tables.decls[id.index()];
        self.decl = id;
        self.unit = decl.unit;
        for sig in 0..tables.sig_count(id) {
            self.bound = Some((id, sig));
            for binder in tables.binders(id, sig) {
                if let Some(bound) = &binder.bound {
                    self.ty(&bound.ty, Use::CONTRA);
                }
            }
            self.bound = None;
        }
        match decl.node {
            DeclNode::Class(class) => {
                for super_ref in &class.super_refs {
                    match self.referent(super_ref.ident.span) {
                        Some(&Referent::Decl(super_decl)) if self.is_type(super_decl) => {
                            self.app(super_decl, &super_ref.args, Use::CO)
                        }
                        _ => {
                            for arg in &super_ref.args {
                                self.ty(arg.ty(), Use::BOTH);
                            }
                        }
                    }
                }
                for member in &class.body.members {
                    if let ClassMember::Field(field) = member
                        && let Some(annot) = &field.ty
                    {
                        let u = match (self.phantom(&annot.ty), field.pub_span) {
                            (true, _) => Use::CO,
                            (false, Some(_)) => Use::BOTH,
                            // Only a method's parameters or results store into a
                            // private field, and they count
                            (false, None) => continue,
                        };
                        self.ty(&annot.ty, u);
                    }
                }
            }
            DeclNode::Alias(alias) => {
                // An alias on a cycle has an erroneous head, already diagnosed
                if let AliasBody::Type(body) = &alias.body
                    && tables.aliases.get(&id) != Some(&Head::Error)
                {
                    self.ty(body, Use::CO);
                }
            }
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for sig in 0..tables.sig_count(id) {
                    self.sig(id, sig);
                }
            }
            DeclNode::Closure(_) => {}
        }

        // A method uses its class's binders, and those the class captures, for it.
        // A private method is called only on `self`, from methods that count, and
        // `(init)` only on an object being constructed.
        if let (DeclNode::Methods(methods), Some((class, _))) = (&decl.node, decl.outer)
            && methods.iter().any(|method| match method.special {
                Some(SpecialMethod::Init) => false,
                Some(_) => true,
                None => method.pub_span.is_some(),
            })
        {
            for binder in self.scope(class, 0) {
                self.constraints
                    .push(((class, binder), Source::Join((id, binder))));
            }
        }

        // Designated opaque aliases take their binders covariantly
        if let Some(Designated::Phantom | Designated::Intrinsic(Intrinsic::Union)) =
            tables.designated.get(&id)
        {
            for slot in 0..tables.binders(id, 0).len() {
                let binder = BinderRef {
                    decl: id,
                    sig: 0,
                    slot,
                };
                self.constraints
                    .push(((id, binder), Source::Path(Use::CO, Vec::new())));
            }
        }
    }

    /// The written binders of a signature and of every declaration enclosing it,
    /// innermost first
    fn scope(&self, decl: DeclId, sig: usize) -> Vec<BinderRef> {
        let mut binders = Vec::new();
        let mut at = Some((decl, sig));
        while let Some((decl, sig)) = at {
            binders.extend(
                (0..self.tables.binders(decl, sig).len()).map(|slot| BinderRef { decl, sig, slot }),
            );
            at = self.tables.decls[decl.index()].outer;
        }
        binders
    }

    fn sig(&mut self, decl: DeclId, sig: usize) {
        let tables = self.tables;
        let completed = &tables.sigs[&(decl, sig)];
        for (index, (_, param)) in completed.params.iter().enumerate() {
            if index == 0 && completed.receiver {
                continue;
            }
            match param {
                ParamTy::Single(slot) | ParamTy::Rest(RestSlot::Items(_, slot)) => {
                    self.slot(slot, Use::CONTRA);
                }
                ParamTy::Rest(RestSlot::Pack(ty) | RestSlot::Pattern(ty)) => {
                    self.ty(ty, Use::CONTRA);
                }
            }
        }
        let func = sig::function(tables, decl, sig);
        for (ambient, written) in [
            (completed.input, &func.input),
            (completed.output, &func.output),
        ] {
            match (ambient, written) {
                (Ambient::Written, Some(implicit)) => self.ty(&implicit.ty, Use::CONTRA),
                (Ambient::Implicit(binder), _) => self.uses(binder, Use::CONTRA),
                _ => {}
            }
        }
        self.slot(&completed.ret, Use::CO);
    }

    fn slot(&mut self, slot: &Slot<'u>, u: Use) {
        if let Slot::Annot(ty) = slot {
            self.ty(ty, u);
        }
    }

    fn referent(&self, head: Span) -> Option<&'t Referent> {
        self.tables.referents.get(&UnitSpan {
            unit: self.unit,
            span: head,
        })
    }

    fn is_type(&self, decl: DeclId) -> bool {
        matches!(
            self.tables.decls[decl.index()].kind,
            DeclKind::Class | DeclKind::Protocol | DeclKind::Alias | DeclKind::OpaqueAlias
        )
    }

    /// Whether a field's type is an application of `std.Phantom`
    fn phantom(&self, mut ty: &TypeExpr) -> bool {
        while let TypeExpr::Group { ty: inner, .. } = ty {
            ty = inner;
        }
        let TypeExpr::App { base, .. } = ty else {
            return false;
        };
        let mut base = &**base;
        while let TypeExpr::Group { ty: inner, .. } = base {
            base = inner;
        }
        matches!(base, TypeExpr::Name { head, .. }
            if matches!(self.referent(*head), Some(Referent::Decl(decl))
                if self.tables.designated.get(decl) == Some(&Designated::Phantom)))
    }

    /// Record a use of a binder at the current position.
    fn uses(&mut self, binder: BinderRef, u: Use) {
        let u = match self.bound == Some((binder.decl, binder.sig)) || self.saturated {
            true => Use::BOTH,
            false => u,
        };
        self.constraints
            .push(((self.decl, binder), Source::Path(u, self.path.clone())));
    }

    /// Walk a type within a position that varies as `key` does.
    fn through(&mut self, key: Key, ty: &'u TypeExpr, u: Use) {
        self.path.push(key);
        self.ty(ty, u);
        self.path.pop();
    }

    /// Record the uses of the binders a type declaration captures, as its implicit
    /// arguments.
    fn captures(&mut self, decl: DeclId, u: Use) {
        let Some((outer, sig)) = self.tables.decls[decl.index()].outer else {
            return;
        };
        for binder in self.scope(outer, sig) {
            self.path.push((decl, binder));
            self.uses(binder, u);
            self.path.pop();
        }
    }

    /// Walk type arguments applied to a type declaration.
    fn app(&mut self, decl: DeclId, args: &'u [TypeArg], u: Use) {
        self.captures(decl, u);
        let fills = self.tables.fill(self.unit, decl, args);
        for (arg, fill) in args.iter().zip(fills) {
            match fill {
                Fill::Binder(slot) | Fill::Item(slot) | Fill::Expand(Some(slot)) => {
                    let binder = BinderRef { decl, sig: 0, slot };
                    self.through((decl, binder), arg.ty(), u);
                }
                Fill::Expand(None) | Fill::Unknown | Fill::Excess | Fill::UnknownKeyword => {
                    self.ty(arg.ty(), Use::BOTH);
                }
            }
        }
    }

    fn ty(&mut self, ty: &'u TypeExpr, u: Use) {
        match ty {
            TypeExpr::Group { ty, .. } => self.ty(ty, u),
            TypeExpr::Name { head, .. } => match self.referent(*head) {
                Some(&Referent::Binder(binder)) => self.uses(binder, u),
                Some(&Referent::Decl(decl)) if self.is_type(decl) => self.captures(decl, u),
                _ => {}
            },
            TypeExpr::App { base, args, .. } => {
                let mut head = &**base;
                while let TypeExpr::Group { ty, .. } = head {
                    head = ty;
                }
                if let TypeExpr::Name { head, .. } = head
                    && let Some(&Referent::Decl(decl)) = self.referent(*head)
                    && self.is_type(decl)
                {
                    return self.app(decl, args, u);
                }
                // What a binder, an external or an erroneous name takes is unknown
                self.ty(base, Use::BOTH);
                for arg in args {
                    self.ty(arg.ty(), Use::BOTH);
                }
            }
            TypeExpr::Schema { params, .. } => self.items(params, u),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    self.ty(member, u);
                }
            }
            TypeExpr::Func {
                params,
                input,
                output,
                arrow_span,
                ret,
                ..
            } => {
                self.items(params, u.flip());
                for implicit in implicits(input, output) {
                    self.ty(&implicit.ty, u.flip());
                }
                self.ty(ret, u);
                let tables = self.tables;
                let ambients = tables.func_ambients.get(&UnitSpan {
                    unit: self.unit,
                    span: *arrow_span,
                });
                for (index, ambient) in ambients.into_iter().flatten().enumerate() {
                    self.ambient(*ambient, index, u.flip());
                }
            }
            TypeExpr::Const { .. } | TypeExpr::Error => {}
        }
    }

    /// Walk the ambient channel a function type without its own takes.
    fn ambient(&mut self, ambient: Ambient, index: usize, u: Use) {
        match ambient {
            Ambient::Implicit(binder) => self.uses(binder, u),
            Ambient::Of(decl, sig) => {
                let func = sig::function(self.tables, decl, sig);
                let Some(implicit) = [&func.input, &func.output][index] else {
                    return;
                };
                // A channel that contains a function type taking that channel is used
                // in every direction its nesting reaches
                if self.expanding.contains(&(decl, sig, index)) {
                    if !self.saturated {
                        self.saturated = true;
                        self.ty(&implicit.ty, u);
                        self.saturated = false;
                    }
                    return;
                }
                self.expanding.push((decl, sig, index));
                self.ty(&implicit.ty, u);
                self.expanding.pop();
            }
            Ambient::Written | Ambient::Unknown => {}
        }
    }

    /// Walk the items of a schema or parameter list.
    fn items(&mut self, params: &'u [TypeParam], u: Use) {
        for param in params {
            match &param.kind {
                Some(TypeParamKind::Pos(ty) | TypeParamKind::Include { ty, .. }) => {
                    self.ty(ty, u);
                }
                Some(TypeParamKind::Key { key, ty, .. }) => {
                    if let TypeKey::Type(key) = key {
                        self.ty(key, u);
                    }
                    self.ty(ty, u);
                }
                Some(TypeParamKind::Open { .. }) | None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
