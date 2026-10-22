//! The IDs a harvest holds, of its unit, declarations, sites, strings, links and
//! modules. They are
//! visited in place: to move a harvest to where it is linked, to cut one down to its
//! surface for a typelib, and to check one read from a typelib.

use super::{
    BinderRef, Decl, DeclNode, Export, Harvest, Link, Module, Referent, Role, Site, Target, Value,
    interface::Kept,
    local,
    surface::{
        Binder, Class, ConstLit, Decorator, LinkId, Member, ModuleId, Name, Param, ParamKind, Ref,
        Signature, SiteId, StrId, Super, Symbol, TypeArg, TypeArgKind, TypeExpr, TypeKey,
        TypeParam,
    },
};
use crate::typeck::r#type::{DeclId, DeclKind, UnitId};

/// What is done with each ID a harvest holds
pub(crate) trait Ids {
    fn unit(&mut self, unit: &mut UnitId);
    fn decl(&mut self, decl: &mut DeclId);
    fn site(&mut self, site: &mut SiteId);
    fn str(&mut self, id: &mut StrId);
    fn link(&mut self, link: &mut LinkId);
    fn module(&mut self, module: &mut ModuleId);
}

impl Harvest<'_> {
    /// Visit every ID the harvest holds.
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

    /// The harvest cut down to its surface: what a unit that imports it can see.
    ///
    /// The surface keeps what [`Harvest::interface`] says it does. Closures are left
    /// out, along with everything declared within a def, method or closure, and the
    /// sites, strings and links only they hold. A decorator naming a declaration left
    /// out names nothing. The whole module table is kept, since a unit reports each
    /// module it imports.
    pub(crate) fn surface(mut self, kept: &Kept) -> Self {
        let decls = &kept.decls;
        for (index, decl) in self.decls.iter_mut().enumerate() {
            let DeclNode::Class(class) = &mut decl.node else {
                continue;
            };
            let class_id = DeclId::from_index(index);
            let mut member = 0;
            class.members.retain(|item| {
                member += 1;
                match item {
                    Member::Field(_) => !kept.fields.contains(&(class_id, member - 1)),
                    Member::Method { decl, .. } => decls[decl.index()],
                }
            });
        }

        let dropped = |target: &Target| match target {
            Target::Local(Referent::Decl(decl)) => !decls[decl.index()],
            _ => false,
        };
        for (decl, _) in self.decls.iter_mut().zip(decls).filter(|(_, keep)| **keep) {
            let decorators: Vec<_> = match &mut decl.node {
                DeclNode::Class(class) => class.decorators.iter_mut().collect(),
                DeclNode::Methods(methods) => (methods.iter_mut())
                    .flat_map(|method| &mut method.decorators)
                    .collect(),
                DeclNode::Alias(_) | DeclNode::Defs(_) | DeclNode::Closure(_) => continue,
            };
            for decorator in decorators {
                if let Decorator::Ident(name) = decorator
                    && name
                        .link
                        .is_some_and(|link| dropped(&self.links[link.index()].base))
                {
                    name.link = None;
                }
            }
        }

        let mut used = Used {
            sites: vec![false; self.sites.len()],
            strs: vec![false; self.strings.len()],
            links: vec![false; self.links.len()],
        };
        for (decl, _) in self.decls.iter_mut().zip(decls).filter(|(_, keep)| **keep) {
            decl.visit(&mut used);
        }
        // Exported variables are on the surface, whether annotated or not
        self.values.visit(&mut used);
        for (site, _) in (self.sites.iter_mut())
            .zip(used.sites.clone())
            .filter(|(_, keep)| *keep)
        {
            site.ty.visit(&mut used);
        }
        for (link, _) in (self.links.iter_mut())
            .zip(used.links.clone())
            .filter(|(_, keep)| *keep)
        {
            link.visit(&mut used);
        }
        self.exports.visit(&mut used);
        self.modules.visit(&mut used);

        let mut renumber = Renumber {
            decls: numbering(decls, DeclId::from_index),
            sites: numbering(&used.sites, SiteId::from_index),
            strs: numbering(&used.strs, StrId::from_index),
            links: numbering(&used.links, LinkId::from_index),
        };
        let targets = (self.links.iter_mut().zip(&used.links))
            .filter(|(_, keep)| **keep)
            .map(|(link, _)| &mut link.base)
            .chain(self.exports.iter_mut().map(|export| &mut export.target));
        for target in targets {
            let Target::Local(referent) = target else {
                continue;
            };
            let (decl, binder) = match referent {
                Referent::Decl(decl) => (*decl, false),
                Referent::Binder(binder) => (binder.decl, true),
                _ => continue,
            };
            // A name the surface reads cannot reach into a body, nor name a type it
            // leaves out. Any other declaration it names is not a type, as is
            // diagnosed.
            if renumber.decls[decl.index()].is_none() {
                let kind = self.decls[decl.index()].kind;
                debug_assert!(
                    !binder && !(kind.nominal() || kind == DeclKind::Alias),
                    "the surface names a type it leaves out"
                );
                *referent = Referent::Error;
            }
        }
        self.decls = retain(self.decls, decls);
        self.sites = retain(self.sites, &used.sites);
        self.strings = retain(self.strings, &used.strs);
        self.links = retain(self.links, &used.links);
        // A typelib has no syntax to look a name up in
        self.refs.clear();
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

/// Finds the sites, strings and links a harvest's surface uses
struct Used {
    sites: Vec<bool>,
    strs: Vec<bool>,
    links: Vec<bool>,
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

    fn link(&mut self, link: &mut LinkId) {
        self.links[link.index()] = true;
    }

    fn module(&mut self, _: &mut ModuleId) {}
}

/// Renumbers the IDs of a harvest cut down to its surface
struct Renumber {
    decls: Vec<Option<DeclId>>,
    sites: Vec<Option<SiteId>>,
    strs: Vec<Option<StrId>>,
    links: Vec<Option<LinkId>>,
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

    fn link(&mut self, link: &mut LinkId) {
        *link = self.links[link.index()].expect("the surface keeps what it refers to");
    }

    fn module(&mut self, _: &mut ModuleId) {}
}

/// Moves a harvest's local unit, declarations, sites and links to where it is
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
