//! How diagnostics show types

use super::Flow;
use crate::typeck::{
    cfg::FuncKind,
    elab::Scoped,
    r#type::{Shown, Style, TypeId},
};

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
