//! Captures: the binders of enclosing declarations that each declaration is lifted
//! over.
//!
//! A declaration needs the outer binders it names anywhere, in its signature,
//! members or body, including the implicit binders a function type written without
//! channels takes. It also needs what each type declaration it names needs, since a
//! reference passes those as leading arguments, and what each declaration nested in
//! it needs, since they are in scope there. A method needs all of its class's
//! binders. A lifted binder keeps its bound, so a declaration also needs what the
//! bound of each binder it needs names. The least solution, less the declaration's
//! own binders, is what it is lifted over, outermost declaration first and in slot
//! order.

use std::collections::{BTreeSet, HashMap};

use super::{Ambient, BinderRef, DeclNode, Referent, Role, Tables, sig};
use crate::{
    ast::{TypeExpr, TypeParam, implicits},
    source::Span,
    typeck::r#type::{DeclId, DeclKind, UnitId, UnitSpan},
};

/// Find the binders every declaration is lifted over.
pub(crate) fn captures(tables: &mut Tables<'_>) {
    let count = tables.decls.len();
    let mut needs = Needs {
        tables: &*tables,
        binders: vec![BTreeSet::new(); count],
        deps: vec![Vec::new(); count],
        bounds: HashMap::new(),
        expanding: Vec::new(),
    };
    for site in &tables.sites {
        if let Some((decl, _)) = site.group() {
            needs.ty(decl.index(), site.unit, site.ty);
        }
        if let Role::Bound(binder) = site.role {
            let node = needs.binders.len();
            needs.binders.push(BTreeSet::new());
            needs.deps.push(Vec::new());
            needs.bounds.insert(binder, node);
            needs.ty(node, site.unit, site.ty);
        }
    }
    for index in 0..count {
        let id = DeclId::from_index(index);
        let decl = &tables.decls[index];
        if let DeclNode::Class(class) = decl.node {
            for super_ref in &class.super_refs {
                needs.name(index, decl.unit, super_ref.ident.span);
                for arg in &super_ref.args {
                    needs.ty(index, decl.unit, arg.ty());
                }
            }
        }
        if let Some((outer, _)) = decl.outer {
            needs.deps[outer.index()].push(id);
            // A method is lifted over its class's whole group
            if let DeclNode::Methods(_) = decl.node {
                needs.deps[index].push(outer);
                for slot in 0..tables.binders(outer, 0).len() {
                    needs.binders[index].insert(BinderRef {
                        decl: outer,
                        sig: 0,
                        slot,
                    });
                }
            }
        }
    }
    let Needs {
        mut binders,
        deps,
        bounds,
        ..
    } = needs;

    // Every set only grows, so iterating to a fixed point finds the least solution
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..binders.len() {
            let mut add = Vec::new();
            // What a declaration needs excludes its own binders
            for dep in &deps[index] {
                for binder in &binders[dep.index()] {
                    if binder.decl != *dep {
                        add.push(*binder);
                    }
                }
            }
            if index < count {
                for binder in &binders[index] {
                    if let Some(&node) = bounds.get(binder)
                        && binder.decl.index() != index
                    {
                        add.extend(binders[node].iter().copied());
                    }
                }
            }
            add.retain(|binder| binder.decl.index() != index && !binders[index].contains(binder));
            changed |= !add.is_empty();
            binders[index].extend(add);
        }
    }

    for (index, set) in binders.into_iter().take(count).enumerate() {
        let id = DeclId::from_index(index);
        let lifted: Vec<_> = set.into_iter().filter(|binder| binder.decl != id).collect();
        debug_assert!(
            lifted
                .iter()
                .all(|binder| encloses(tables, binder.decl, binder.sig, id)),
            "{id:?} captures a binder of a declaration that does not enclose it"
        );
        tables.lifted.insert(id, lifted);
    }
}

/// Whether the signature `sig` of `outer` encloses `decl`
fn encloses(tables: &Tables<'_>, outer: DeclId, sig: usize, decl: DeclId) -> bool {
    let mut at = tables.decls[decl.index()].outer;
    while let Some(found) = at {
        if found == (outer, sig) {
            return true;
        }
        at = tables.decls[found.0.index()].outer;
    }
    false
}

struct Needs<'t, 'u> {
    tables: &'t Tables<'u>,
    /// The binders each declaration, and then each binder's bound, names directly,
    /// and then all it needs
    binders: Vec<BTreeSet<BinderRef>>,
    /// The declarations whose needs each declaration or bound shares
    deps: Vec<Vec<DeclId>>,
    /// Each bounded binder's entry in `binders` and `deps`
    bounds: HashMap<BinderRef, usize>,
    /// The written channels being walked in place of a function type's own
    expanding: Vec<(DeclId, usize, usize)>,
}

impl Needs<'_, '_> {
    fn name(&mut self, node: usize, unit: UnitId, head: Span) {
        let tables = self.tables;
        match tables.referents.get(&UnitSpan { unit, span: head }) {
            Some(&Referent::Binder(binder)) => {
                self.binders[node].insert(binder);
            }
            Some(&Referent::Decl(named))
                if matches!(
                    tables.decls[named.index()].kind,
                    DeclKind::Class | DeclKind::Protocol | DeclKind::Alias | DeclKind::OpaqueAlias
                ) =>
            {
                self.deps[node].push(named);
            }
            _ => {}
        }
    }

    fn ty(&mut self, node: usize, unit: UnitId, ty: &TypeExpr) {
        match ty {
            TypeExpr::Name { head, .. } => self.name(node, unit, *head),
            TypeExpr::Const { .. } | TypeExpr::Error => {}
            TypeExpr::App { base, args, .. } => {
                self.ty(node, unit, base);
                for arg in args {
                    self.ty(node, unit, arg.ty());
                }
            }
            TypeExpr::Schema { params, .. } => self.params(node, unit, params),
            TypeExpr::Group { ty, .. } => self.ty(node, unit, ty),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    self.ty(node, unit, member);
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
                self.params(node, unit, params);
                for implicit in implicits(input, output) {
                    self.ty(node, unit, &implicit.ty);
                }
                self.ty(node, unit, ret);
                let tables = self.tables;
                let ambients = tables.func_ambients.get(&UnitSpan {
                    unit,
                    span: *arrow_span,
                });
                for (index, ambient) in ambients.into_iter().flatten().enumerate() {
                    self.ambient(node, *ambient, index);
                }
            }
        }
    }

    fn params(&mut self, node: usize, unit: UnitId, params: &[TypeParam]) {
        for ty in params.iter().flat_map(TypeParam::tys) {
            self.ty(node, unit, ty);
        }
    }

    fn ambient(&mut self, node: usize, ambient: Ambient, index: usize) {
        match ambient {
            Ambient::Implicit(binder) => {
                self.binders[node].insert(binder);
            }
            Ambient::Of(owner, sig) => {
                let func = sig::function(self.tables, owner, sig);
                let Some(implicit) = [&func.input, &func.output][index] else {
                    return;
                };
                if self.expanding.contains(&(owner, sig, index)) {
                    return;
                }
                self.expanding.push((owner, sig, index));
                let unit = self.tables.decls[owner.index()].unit;
                self.ty(node, unit, &implicit.ty);
                self.expanding.pop();
            }
            Ambient::Written | Ambient::Unknown => {}
        }
    }
}
