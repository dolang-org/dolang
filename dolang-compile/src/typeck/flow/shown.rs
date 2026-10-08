//! How diagnostics show types

use std::borrow::Cow;

use super::Flow;
use crate::typeck::{
    cfg::FuncKind,
    elab::Tables,
    r#type::{Collection, DeclId, Names, Shown, Style, TypeId},
};

/// What a diagnostic about the running block's function sees
pub(super) struct Scoped<'a, 'u> {
    tables: &'a Tables<'u>,
    /// The declarations whose binders the function is within
    scope: Vec<DeclId>,
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

impl<'u> Flow<'_, 'u> {
    /// What a diagnostic about the running block's function sees
    pub(super) fn names(&self) -> Scoped<'_, 'u> {
        let tables = self.tables;
        let mut scope = Vec::new();
        let mut func = self.current;
        while let Some(id) = func {
            let data = self.ir.func(id);
            if let FuncKind::Decl(decl) = data.kind {
                let key = (decl, tables.primary_sig(decl));
                for binder in tables.groups.get(&key).into_iter().flatten() {
                    let owner = tables.sig_decls[&(binder.decl, binder.sig)];
                    if !scope.contains(&owner) {
                        scope.push(owner);
                    }
                }
            }
            func = data.parent;
        }
        Scoped { tables, scope }
    }

    /// A type as a diagnostic shows it
    pub(super) fn shown(&self, ty: TypeId) -> String {
        self.db.render(ty, &self.names(), Style::Reader)
    }

    /// A type as the subject of a diagnostic's sentence
    pub(super) fn subject(&self, ty: TypeId) -> Shown {
        self.db.subject(ty, &self.names(), Style::Reader)
    }

    /// A type found where another was expected, as a diagnostic shows them
    pub(super) fn pair(&self, found: TypeId, expected: TypeId) -> (Shown, Shown) {
        self.db.pair(found, expected, &self.names(), Style::Reader)
    }
}
