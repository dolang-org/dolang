//! The IDs a shard and its facts hold, of its unit, declarations, sites, strings,
//! links and modules. They are visited in place: to move a shard to where it is
//! linked, and to check one read from a typelib.

use std::{hash::Hash, mem};

use rustc_hash::FxHashMap;

use super::{
    Ambient, BinderRef, Decl, DeclNode, Export, Facts, Head, KindOf, Link, Module, ParamTy,
    Referent, RestSlot, Role, Shard, Sig, Site, Slot, Target, Value, local,
    surface::{
        Binder, Class, ConstLit, Decorator, LinkId, Member, ModuleId, Name, Param, ParamKind, Ref,
        Signature, SiteId, StrId, Super, Symbol, TypeArg, TypeArgKind, TypeExpr, TypeKey,
        TypeParam,
    },
};
use crate::{
    source::Span,
    typeck::r#type::{DeclId, UnitId, Variance},
};

/// What is done with each ID a shard holds
pub(crate) trait Ids {
    fn unit(&mut self, unit: &mut UnitId);
    fn decl(&mut self, decl: &mut DeclId);
    fn site(&mut self, site: &mut SiteId);
    fn str(&mut self, id: &mut StrId);
    fn link(&mut self, link: &mut LinkId);
    fn module(&mut self, module: &mut ModuleId);
}

impl Shard<'_> {
    /// Visit every ID collection fills the shard with. What linking fills is empty
    /// until then.
    pub(crate) fn visit_ids(&mut self, ids: &mut impl Ids) {
        self.decls.visit(ids);
        self.sites.visit(ids);
        self.links.visit(ids);
        for link in self.refs.values_mut() {
            ids.link(link);
        }
        self.exports.visit(ids);
        self.values.visit(ids);
        self.modules.visit(ids);
    }
}

impl Facts {
    /// Visit every ID the facts hold.
    pub(crate) fn visit_ids(&mut self, ids: &mut impl Ids) {
        let Facts {
            aliases,
            binder_kinds,
            alias_kinds,
            sigs,
            fields,
            variance,
            captured,
            lifted,
            groups,
        } = self;
        aliases.visit(ids);
        binder_kinds.visit(ids);
        alias_kinds.visit(ids);
        sigs.visit(ids);
        fields.visit(ids);
        variance.visit(ids);
        captured.visit(ids);
        lifted.visit(ids);
        groups.visit(ids);
    }
}

/// Moves a shard's local unit, declarations, sites and links to where it is
/// linked. Strings and modules stay its own.
pub(crate) struct Rebase {
    pub(crate) unit: UnitId,
    /// The first declaration of the unit
    pub(crate) decls: usize,
    /// The first site of the unit
    pub(crate) sites: usize,
    /// The first link of the unit
    pub(crate) links: usize,
}

impl Ids for Rebase {
    fn unit(&mut self, unit: &mut UnitId) {
        debug_assert_eq!(*unit, local(), "a shard refers to its own unit");
        *unit = self.unit;
    }

    fn decl(&mut self, decl: &mut DeclId) {
        *decl = DeclId::from_index(self.decls + decl.index());
    }

    fn site(&mut self, site: &mut SiteId) {
        *site = SiteId::from_index(self.sites + site.index());
    }

    fn str(&mut self, _: &mut StrId) {}

    fn link(&mut self, link: &mut LinkId) {
        *link = LinkId::from_index(self.links + link.index());
    }

    fn module(&mut self, _: &mut ModuleId) {}
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

/// A map whose keys may hold IDs, rebuilt with its keys visited
impl<K: Visit + Eq + Hash, V: Visit> Visit for FxHashMap<K, V> {
    fn visit(&mut self, ids: &mut impl Ids) {
        *self = mem::take(self)
            .into_iter()
            .map(|(mut key, mut value)| {
                key.visit(ids);
                value.visit(ids);
                (key, value)
            })
            .collect();
    }
}

impl Visit for (DeclId, Span) {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.decl(&mut self.0);
    }
}

impl Visit for (DeclId, BinderRef) {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.decl(&mut self.0);
        self.1.visit(ids);
    }
}

/// Something that holds no IDs
macro_rules! no_ids {
    ($($ty:ty),*) => {
        $(impl Visit for $ty {
            fn visit(&mut self, _: &mut impl Ids) {}
        })*
    };
}

no_ids!(KindOf, Variance);

impl Visit for Head {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Head::Decl(decl) => decl.visit(ids),
            Head::Binder(binder) => binder.visit(ids),
            Head::Structural | Head::Error => {}
        }
    }
}

impl Visit for Slot {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Slot::Annot(site) => site.visit(ids),
            Slot::Unknown | Slot::SelfType | Slot::ObjectType | Slot::Nil => {}
        }
    }
}

impl Visit for ParamTy {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            ParamTy::Single(slot) | ParamTy::Rest(RestSlot::Items(_, slot)) => slot.visit(ids),
            ParamTy::Rest(RestSlot::Pack(site) | RestSlot::Pattern(site)) => site.visit(ids),
        }
    }
}

impl Visit for Ambient {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Ambient::Implicit(binder) => binder.visit(ids),
            Ambient::Of(decl, _) => ids.decl(decl),
            Ambient::Written | Ambient::Value => {}
        }
    }
}

impl Visit for Sig {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.params.visit(ids);
        self.input.visit(ids);
        self.output.visit(ids);
        self.ret.visit(ids);
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

impl Visit for LinkId {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.link(self);
    }
}

impl Visit for Name {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.str(&mut self.text);
    }
}

impl Visit for Symbol {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.str(&mut self.text);
    }
}

impl Visit for Ref {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.link.visit(ids);
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
                decorators,
            }) => {
                binders.visit(ids);
                supers.visit(ids);
                members.visit(ids);
                for decorator in decorators {
                    if let Decorator::Ident(name) = decorator {
                        name.visit(ids);
                    }
                }
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
            ConstLit::Str(value) => ids.str(value),
            ConstLit::Int(_) | ConstLit::Bool(_) | ConstLit::Nil => {}
        }
    }
}

impl Visit for Super {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.head.visit(ids);
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
            TypeExpr::Name(head) => head.visit(ids),
            TypeExpr::Const { value, .. } => value.visit(ids),
            TypeExpr::App { base, args, .. } => {
                base.visit(ids);
                args.visit(ids);
            }
            TypeExpr::Schema { params, .. }
            | TypeExpr::Tuple { params, .. }
            | TypeExpr::Record { params, .. } => params.visit(ids),
            TypeExpr::Group { ty, .. } | TypeExpr::Array { elem: ty, .. } => ty.visit(ids),
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
        match self {
            TypeParam::Pos { ty, .. } | TypeParam::AnyKey(Some(ty)) => ty.visit(ids),
            TypeParam::Key { key, ty, .. } => {
                match key {
                    TypeKey::Sym(name) => name.visit(ids),
                    TypeKey::Type(key) => key.visit(ids),
                }
                ty.visit(ids);
            }
            TypeParam::AnyPos | TypeParam::AnyKey(None) | TypeParam::Open(_) => {}
        }
    }
}

impl Visit for Target {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Target::Local(referent) => referent.visit(ids),
            Target::Import { module, item } => {
                ids.module(module);
                ids.str(item);
            }
            Target::Module(module) => ids.module(module),
        }
    }
}

impl Visit for Link {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.unit(&mut self.unit);
        self.base.visit(ids);
        if let Some(item) = &mut self.item {
            ids.str(item);
        }
    }
}

impl Visit for Module {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.str(&mut self.name);
    }
}

impl Visit for Export {
    fn visit(&mut self, ids: &mut impl Ids) {
        ids.str(&mut self.name);
        self.target.visit(ids);
    }
}

impl Visit for Value {
    fn visit(&mut self, ids: &mut impl Ids) {
        self.annot.visit(ids);
    }
}

impl Visit for Referent {
    fn visit(&mut self, ids: &mut impl Ids) {
        match self {
            Referent::Decl(decl) => decl.visit(ids),
            Referent::Binder(binder) => binder.visit(ids),
            Referent::Module(unit) => ids.unit(unit),
            Referent::Value(span) => ids.unit(&mut span.unit),
            Referent::Error => {}
        }
    }
}
