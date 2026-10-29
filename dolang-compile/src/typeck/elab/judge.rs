//! What elaboration concluded about source spans, for an analysis, and the
//! names the tables give declarations.

use std::{borrow::Cow, sync::Arc};

use rustc_hash::FxHashMap;

use super::{Ambient, BinderRef, DeclNode, Designated, Referent, Tables};
use crate::{
    diag::Span,
    typeck::{
        analysis::Entry,
        flow::Role,
        r#type::{
            Collection, Copier, Database, DeclId, Intrinsic, Names, Style, Type, TypeId, UnitId,
        },
    },
};

impl Tables<'_> {
    /// The analysis's entries for `unit`'s references, written types and
    /// declarations, copied from `db` by `copier`. A reference to something other
    /// than a declaration or a binder has none.
    pub(crate) fn entries(
        &self,
        db: &Database,
        copier: &mut Copier<'_>,
        unit: UnitId,
    ) -> Vec<(Span, Entry)> {
        let entry = |role, ty| Entry {
            role,
            name: None,
            ty,
            unassigned: false,
            decl: None,
            overload: None,
            item: None,
            group: None,
        };
        let mut entries = Vec::new();
        let declare = |entries: &mut Vec<_>, copier: &mut Copier<'_>, span, id: DeclId| {
            let decl = Some(copier.declaration(id));
            let ty = copier.ty(db.declaration(id).ty);
            entries.push((
                span,
                Entry {
                    decl,
                    ..entry(Role::Declaration, ty)
                },
            ));
        };
        let shard = self.shard(unit);
        let mut sigs: Vec<_> = shard.sig_decls.iter().collect();
        sigs.sort_unstable();
        for (&(id, sig), &db_id) in sigs {
            let span = match &self.decl(id).node {
                DeclNode::Defs(defs) => defs[sig].name.span,
                DeclNode::Methods(methods) => methods[sig].name.span,
                _ => continue,
            };
            declare(&mut entries, copier, span, db_id);
        }
        for id in self.unit_decl_ids(unit) {
            let decl = self.decl(id);
            if !matches!(decl.node, DeclNode::Class(_) | DeclNode::Alias(_)) {
                continue;
            }
            let span = decl.name_span().expect("a class or alias is named");
            declare(&mut entries, copier, span, id);
        }
        let mut groups: FxHashMap<(DeclId, usize), Arc<[String]>> = FxHashMap::default();
        for (_, site) in self.unit_sites(unit) {
            let ty = site.interned.get().expect("population interns each site");
            let group = site.group().map(|key| {
                let names = groups.entry(key).or_insert_with(|| {
                    self.group(key)
                        .expect("population groups each signature")
                        .iter()
                        .map(|binder| self.binder_name(*binder))
                        .collect()
                });
                Arc::clone(names)
            });
            let ty = copier.ty(ty);
            entries.push((
                site.ty.span(),
                Entry {
                    group,
                    ..entry(Role::Annotation, ty)
                },
            ));
        }
        let file = &self
            .info(unit)
            .source
            .expect("a unit judged has source")
            .file;
        let mut refs: Vec<_> = shard.refs.iter().collect();
        refs.sort_unstable_by_key(|&(head, _)| (head.start, head.end));
        for (&head, &link) in refs {
            let head = file.locate(head);
            let (decl, name, ty) = match *self.linked(Some(link)).expect("a link refers") {
                Referent::Decl(id) => {
                    let ty = copier.ty(db.intern(Type::Decl(id)));
                    (Some(copier.declaration(id)), None, ty)
                }
                Referent::Binder(binder) => {
                    let name = self.binder_name(binder).into_boxed_str();
                    (None, Some(name), copier.ty(db.top()))
                }
                Referent::Module(_) | Referent::Value(_) | Referent::Error => continue,
            };
            entries.push((
                head,
                Entry {
                    decl,
                    name,
                    ..entry(Role::Reference, ty)
                },
            ));
        }
        entries
    }

    /// Where a declaration is: its unit's module name, and its name in the
    /// module, qualified by the declarations it is nested in. `None` for a
    /// database declaration of a signature.
    pub(crate) fn place(&self, id: DeclId) -> Option<(String, String)> {
        let unit = self.try_decl(id)?.unit;
        let module = self.unit_name(unit);
        let mut name = self.qualified(id);
        name.drain(..=module.len());
        Some((module, name))
    }

    /// A declaration's name, qualified by its unit and the declarations it is
    /// nested in
    pub(crate) fn qualified(&self, id: DeclId) -> String {
        let decl = self.decl(id);
        let mut name = match decl.outer {
            Some((outer, _)) => self.qualified(outer),
            None => self.unit_name(decl.unit),
        };
        name.push('.');
        match decl.name {
            Some(spelled) => name.push_str(self.name(decl.unit, spelled)),
            None => name.push_str("<closure>"),
        }
        name
    }

    /// A database declaration's name: a declaration's qualified name, or for one
    /// of the declarations of a def's or method's signatures, its name and the
    /// signature's index
    fn declared(&self, id: DeclId) -> String {
        match self.sig_owner(id) {
            Some((decl, sig)) => format!("{}#{sig}", self.qualified(decl)),
            None => self.qualified(id),
        }
    }

    /// A module's name, or a script's file stem
    fn unit_name(&self, unit: UnitId) -> String {
        self.info(unit).name()
    }

    /// A closed type
    #[cfg_attr(
        not(any(test, feature = "debug")),
        expect(dead_code, reason = "used by tests and traces")
    )]
    pub(crate) fn render_type(&self, db: &Database, ty: TypeId) -> String {
        db.render(ty, self, Style::Full)
    }

    /// The name of a binder, or `in` or `out` for an implicit one
    fn binder_name(&self, binder: BinderRef) -> String {
        let unit = self.decl(binder.decl).unit;
        match self.binders(binder.decl, binder.sig).get(binder.slot) {
            Some(written) => self.name(unit, written.name).to_owned(),
            None => match self
                .sig(binder.decl, binder.sig)
                .expect("a binder past the written ones is a signature's")
                .input
            {
                Ambient::Implicit(input) if input == binder => "in".to_owned(),
                _ => "out".to_owned(),
            },
        }
    }
}

impl Names for Tables<'_> {
    fn declaration(&self, id: DeclId) -> Cow<'_, str> {
        Cow::Owned(self.declared(id))
    }

    fn collection(&self, id: DeclId) -> Option<Collection> {
        match self.designated.get(&id)? {
            Designated::Array => Some(Collection::Array),
            Designated::Intrinsic(Intrinsic::Tuple) => Some(Collection::Tuple),
            Designated::Record => Some(Collection::Record),
            Designated::Dict => Some(Collection::Dict),
            _ => None,
        }
    }
}

/// The tables' names, with the binders of the declarations in `scope` named bare,
/// as a diagnostic within them shows them
pub(crate) struct Scoped<'a, 'u> {
    pub(crate) tables: &'a Tables<'u>,
    pub(crate) scope: Vec<DeclId>,
}

impl Names for Scoped<'_, '_> {
    fn declaration(&self, id: DeclId) -> Cow<'_, str> {
        self.tables.declaration(id)
    }

    fn collection(&self, id: DeclId) -> Option<Collection> {
        self.tables.collection(id)
    }

    fn in_scope(&self, decl: DeclId) -> bool {
        self.scope.contains(&decl)
    }
}
