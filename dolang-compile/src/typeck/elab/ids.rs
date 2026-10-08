//! The IDs a harvest holds, of its unit, declarations, sites and strings. They are
//! visited in place: to move a harvest to where it is linked, to cut one down to its
//! surface for a typelib, and to check one read from a typelib.

use std::collections::HashSet;

use super::{
    BinderRef, Decl, DeclNode, Harvest, ModuleRef, Referent, Role, Site, Target, local,
    surface::{
        Binder, Class, ConstLit, Decorator, Member, Name, Param, ParamKind, Signature, SiteId,
        StrId, Super, TypeArg, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeParamKind,
    },
};
use crate::typeck::r#type::{DeclId, DeclKind, UnitId};

/// What is done with each ID a harvest holds
pub(crate) trait Ids {
    fn unit(&mut self, unit: &mut UnitId);
    fn decl(&mut self, decl: &mut DeclId);
    fn site(&mut self, site: &mut SiteId);
    fn str(&mut self, id: &mut StrId);
}

impl Harvest<'_> {
    /// Visit every ID the harvest holds.
    pub(crate) fn visit_ids(&mut self, ids: &mut impl Ids) {
        self.decls.visit(ids);
        self.sites.visit(ids);
        for pending in &mut self.pending {
            ids.unit(&mut pending.head.unit);
            pending.base.visit(ids);
        }
        for (_, target) in self.exports.values_mut() {
            target.visit(ids);
        }
        for site in self.values.values_mut() {
            site.visit(ids);
        }
    }

    /// The harvest cut down to its surface: what a unit that imports it can see.
    ///
    /// Closures are left out, along with everything declared within a def, method
    /// or closure, and the sites, strings and type names only they hold.
    pub(crate) fn surface(mut self) -> Self {
        let mut decls = Vec::with_capacity(self.decls.len());
        for decl in &self.decls {
            // An outer declaration is allocated first
            let keep = decl.kind != DeclKind::Closure
                && decl.outer.is_none_or(|(outer, _)| {
                    decls[outer.index()]
                        && matches!(self.decls[outer.index()].node, DeclNode::Class(_))
                });
            decls.push(keep);
        }

        let mut used = Used {
            sites: vec![false; self.sites.len()],
            strs: vec![false; self.strings.len()],
        };
        // The type names whose resolutions the surface reads, by head
        let mut heads = HashSet::new();
        for (decl, _) in self.decls.iter_mut().zip(&decls).filter(|(_, keep)| **keep) {
            decl.visit(&mut used);
            match &decl.node {
                DeclNode::Class(class) => {
                    for supertype in &class.supers {
                        heads.insert(supertype.head.span);
                        for arg in &supertype.args {
                            arg.ty().names(&mut |head, _| {
                                heads.insert(head.span);
                            });
                        }
                    }
                }
                DeclNode::Methods(methods) => {
                    for decorator in methods.iter().flat_map(|method| &method.decorators) {
                        if let Decorator::Ident(name) = decorator {
                            heads.insert(name.span);
                        }
                    }
                }
                DeclNode::Alias(_) | DeclNode::Defs(_) | DeclNode::Closure(_) => {}
            }
        }
        // Exported variables are on the surface, whether annotated or not
        for site in self.values.values_mut() {
            site.visit(&mut used);
        }
        for (site, _) in (self.sites.iter_mut())
            .zip(used.sites.clone())
            .filter(|(_, keep)| *keep)
        {
            site.ty.names(&mut |head, _| {
                heads.insert(head.span);
            });
            site.ty.visit(&mut used);
        }

        let mut renumber = Renumber {
            decls: numbering(&decls, DeclId::from_index),
            sites: numbering(&used.sites, SiteId::from_index),
            strs: numbering(&used.strs, StrId::from_index),
        };
        self.pending
            .retain(|pending| heads.contains(&pending.head.span));
        let targets = (self.pending.iter_mut().map(|pending| &mut pending.base))
            .chain(self.exports.values_mut().map(|(_, target)| target));
        for target in targets {
            let Target::Local(referent) = target else {
                continue;
            };
            let decl = match referent {
                Referent::Decl(decl) => *decl,
                Referent::Binder(binder) => binder.decl,
                _ => continue,
            };
            // A name the surface reads cannot reach into a body
            if renumber.decls[decl.index()].is_none() {
                debug_assert!(false, "the surface names a declaration it leaves out");
                *referent = Referent::Error;
            }
        }
        self.decls = retain(self.decls, &decls);
        self.sites = retain(self.sites, &used.sites);
        self.strings = retain(self.strings, &used.strs);
        self.visit_ids(&mut renumber);
        self
    }
}

/// The new ID of each item kept, in order
fn numbering<T>(keep: &[bool], id: impl Fn(usize) -> T) -> Vec<Option<T>> {
    let mut next = 0;
    keep.iter()
        .map(|&keep| {
            keep.then(|| {
                next += 1;
                id(next - 1)
            })
        })
        .collect()
}

fn retain<T>(items: Vec<T>, keep: &[bool]) -> Vec<T> {
    items
        .into_iter()
        .zip(keep)
        .filter_map(|(item, &keep)| keep.then_some(item))
        .collect()
}

/// Finds the sites and strings a harvest's surface uses
struct Used {
    sites: Vec<bool>,
    strs: Vec<bool>,
}

impl Ids for Used {
    fn unit(&mut self, _: &mut UnitId) {}

    fn decl(&mut self, _: &mut DeclId) {}

    fn site(&mut self, site: &mut SiteId) {
        self.sites[site.index()] = true;
    }

    fn str(&mut self, id: &mut StrId) {
        self.strs[id.index()] = true;
    }
}

/// Renumbers the IDs of a harvest cut down to its surface
struct Renumber {
    decls: Vec<Option<DeclId>>,
    sites: Vec<Option<SiteId>>,
    strs: Vec<Option<StrId>>,
}

impl Ids for Renumber {
    fn unit(&mut self, _: &mut UnitId) {}

    fn decl(&mut self, decl: &mut DeclId) {
        *decl = self.decls[decl.index()].expect("the surface keeps what it refers to");
    }

    fn site(&mut self, site: &mut SiteId) {
        *site = self.sites[site.index()].expect("the surface keeps what it refers to");
    }

    fn str(&mut self, id: &mut StrId) {
        *id = self.strs[id.index()].expect("the surface keeps what it refers to");
    }
}

/// Moves a harvest's local unit, declarations and sites to where it is linked
pub(crate) struct Rebase {
    pub(crate) unit: UnitId,
    /// The first declaration of the unit
    pub(crate) decls: usize,
    /// The first site of the unit
    pub(crate) sites: usize,
}

impl Ids for Rebase {
    fn unit(&mut self, unit: &mut UnitId) {
        debug_assert_eq!(*unit, local(), "a harvest refers to its own unit");
        *unit = self.unit;
    }

    fn decl(&mut self, decl: &mut DeclId) {
        *decl = DeclId::from_index(self.decls + decl.index());
    }

    fn site(&mut self, site: &mut SiteId) {
        *site = SiteId::from_index(self.sites + site.index());
    }

    fn str(&mut self, _: &mut StrId) {}
}

/// Something that holds IDs
trait Visit {
    fn visit(&mut self, ids: &mut impl Ids);
}

impl<T: Visit> Visit for Vec<T> {
    fn visit(&mut self, ids: &mut impl Ids) {
        for item in self {
            item.visit(ids);
        }
    }
}

impl<T: Visit> Visit for Option<T> {
    fn visit(&mut self, ids: &mut impl Ids) {
        if let Some(item) = self {
            item.visit(ids);
        }
    }
}

impl<T: Visit> Visit for Box<T> {
    fn visit(&mut self, ids: &mut impl Ids) {
        (**self).visit(ids);
    }
}

impl Visit for DeclId {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.decl(self);
    }
}

impl Visit for SiteId {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.site(self);
    }
}

impl Visit for Name {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.str(&mut self.text);
    }
}

/// A declaration signature
impl Visit for (DeclId, usize) {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.decl(&mut self.0);
    }
}

impl Visit for BinderRef {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.decl(&mut self.decl);
    }
}

impl Visit for Decl<'_> {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.unit(&mut self.unit);
        self.name.visit(ids);
        self.node.visit(ids);
        self.outer.visit(ids);
    }
}

impl Visit for DeclNode {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            DeclNode::Class(Class {
                binders,
                supers,
                members,
            }) => {
                binders.visit(ids);
                supers.visit(ids);
                members.visit(ids);
            }
            DeclNode::Alias(alias) => {
                alias.binders.visit(ids);
                alias.body.visit(ids);
            }
            DeclNode::Defs(defs) => {
                for def in defs {
                    def.name.visit(ids);
                    def.binders.visit(ids);
                    def.sig.visit(ids);
                }
            }
            DeclNode::Methods(methods) => {
                for method in methods {
                    method.name.visit(ids);
                    method.binders.visit(ids);
                    method.sig.visit(ids);
                    for decorator in &mut method.decorators {
                        if let Decorator::Ident(name) = decorator {
                            name.visit(ids);
                        }
                    }
                }
            }
            DeclNode::Closure(closure) => closure.sig.visit(ids),
        }
    }
}

impl Visit for Binder {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.name.visit(ids);
        self.bound.visit(ids);
        self.default.visit(ids);
    }
}

impl Visit for Signature {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.params.visit(ids);
        self.input.visit(ids);
        self.output.visit(ids);
        self.ret.visit(ids);
    }
}

impl Visit for Param {
    fn visit(&mut self, ids: &mut impl Ids) {
        match &mut self.kind {
            ParamKind::Key { key } => key.visit(ids),
            ParamKind::ConstKey { key } => key.visit(ids),
            ParamKind::Pos | ParamKind::Rest { .. } => {}
        }
        self.name.visit(ids);
        self.annot.visit(ids);
    }
}

impl Visit for ConstLit {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            ConstLit::Sym(name) => name.visit(ids),
            ConstLit::Str(_) | ConstLit::Int(_) | ConstLit::Bool(_) | ConstLit::Nil => {}
        }
    }
}

impl Visit for Super {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.head.visit(ids);
        self.fields.visit(ids);
        self.args.visit(ids);
    }
}

impl Visit for Member {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Member::Field(field) => {
                field.names.visit(ids);
                field.annot.visit(ids);
            }
            Member::Method { decl, .. } => decl.visit(ids),
        }
    }
}

impl Visit for Site {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.unit(&mut self.unit);
        self.ty.visit(ids);
        match &mut self.role {
            Role::Bound(binder) | Role::Default(binder) => binder.visit(ids),
            Role::Alias(decl) => decl.visit(ids),
            Role::Type | Role::Rest | Role::Pattern => {}
        }
        self.ambient.visit(ids);
        self.owner.visit(ids);
    }
}

impl Visit for TypeExpr {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            TypeExpr::Name { head, fields, .. } => {
                head.visit(ids);
                fields.visit(ids);
            }
            TypeExpr::Const { value, .. } => value.visit(ids),
            TypeExpr::App { base, args, .. } => {
                base.visit(ids);
                args.visit(ids);
            }
            TypeExpr::Schema { params, .. }
            | TypeExpr::Tuple { params, .. }
            | TypeExpr::Record { params, .. } => params.visit(ids),
            TypeExpr::Group { ty, .. } => ty.visit(ids),
            TypeExpr::Union { members, .. } => members.visit(ids),
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                params.visit(ids);
                input.visit(ids);
                output.visit(ids);
                ret.visit(ids);
            }
            TypeExpr::Error { .. } => {}
        }
    }
}

impl Visit for TypeArg {
    fn visit(&mut self, ids: &mut impl Ids) {
        match &mut self.kind {
            TypeArgKind::Pos(ty) | TypeArgKind::Expand { ty } => ty.visit(ids),
            TypeArgKind::Key { name, ty } => {
                name.visit(ids);
                ty.visit(ids);
            }
        }
    }
}

impl Visit for TypeParam {
    fn visit(&mut self, ids: &mut impl Ids) {
        match &mut self.kind {
            Some(TypeParamKind::Pos(ty)) | Some(TypeParamKind::Include { ty }) => ty.visit(ids),
            Some(TypeParamKind::Key { key, ty }) => {
                match key {
                    TypeKey::Sym(name) => name.visit(ids),
                    TypeKey::Type(key) => key.visit(ids),
                }
                ty.visit(ids);
            }
            Some(TypeParamKind::Open(_)) | None => {}
        }
    }
}

impl Visit for Target<'_> {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Target::Local(referent) => referent.visit(ids),
            Target::Import { .. } | Target::Module(_) => {}
        }
    }
}

impl Visit for Referent {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Referent::Decl(decl) => decl.visit(ids),
            Referent::Binder(binder) => binder.visit(ids),
            Referent::Module(ModuleRef::Unit(unit)) => ids.unit(unit),
            Referent::Value(span) => ids.unit(&mut span.unit),
            Referent::External { .. }
            | Referent::Module(ModuleRef::External(_))
            | Referent::Error => {}
        }
    }
}
