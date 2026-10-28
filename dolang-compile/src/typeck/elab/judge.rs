//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use std::{borrow::Cow, fmt::Write};

use super::{Ambient, BinderRef, DeclNode, Designated, Referent, Tables};
use crate::{
    source::Span,
    typeck::r#type::{
        BinderOrigin, Collection, Database, DeclId, Declaration, Intrinsic, Names, Style, Type,
        TypeId, UnitId, Variance,
    },
};

/// The judgments the tables record, by the name a fixture writes
pub(crate) const JUDGMENTS: &[&str] = &[
    "ref", "decl", "type", "flow", "value", "expected", "reached", "callee", "item",
];

/// A binder's variance, as a suffix of its name: nothing for invariant
fn variance(variance: Variance) -> &'static str {
    match variance {
        Variance::Covariant => " +",
        Variance::Contravariant => " -",
        Variance::Invariant => "",
    }
}

impl Tables<'_> {
    /// Every judgment about spans of `unit`, in source order
    pub(crate) fn judgments(
        &self,
        db: &Database,
        unit: UnitId,
    ) -> Vec<(&'static str, Span, String)> {
        let mut judgments = Vec::new();
        self.populated(db, unit, &mut judgments);
        for (&head, &link) in &self.refs[unit.index()] {
            let referent = &self.referents[link.index()];
            judgments.push(("ref", head, self.referent(referent)));
        }
        judgments.sort_by_key(|(name, span, _)| (span.start, span.end, *name));
        judgments
    }

    fn referent(&self, referent: &Referent) -> String {
        match referent {
            Referent::Decl(id) => self.qualified(*id),
            Referent::Binder(binder) => self.binder(*binder),
            Referent::Module(unit) => format!("module {}", self.unit_name(*unit)),
            Referent::Value(_) => "value".to_owned(),
            Referent::Error => "error".to_owned(),
        }
    }

    /// Where a declaration is: its unit's module name, and its name in the
    /// module, qualified by the declarations it is nested in. `None` for a
    /// database declaration of a signature.
    pub(crate) fn place(&self, id: DeclId) -> Option<(String, String)> {
        let unit = self.decls.get(id.index())?.unit;
        let module = self.unit_name(unit);
        let mut name = self.qualified(id);
        name.drain(..=module.len());
        Some((module, name))
    }

    /// A declaration's name, qualified by its unit and the declarations it is
    /// nested in
    pub(crate) fn qualified(&self, id: DeclId) -> String {
        let decl = &self.decls[id.index()];
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
        if id.index() < self.decls.len() {
            return self.qualified(id);
        }
        let sig = self
            .sig_decls
            .iter()
            .find(|&(_, &declaration)| declaration == id);
        match sig {
            Some((&(decl, sig), _)) => format!("{}#{sig}", self.qualified(decl)),
            None => unreachable!("every database declaration is a declaration's or a signature's"),
        }
    }

    /// A module's name, or a script's file stem
    fn unit_name(&self, unit: UnitId) -> String {
        self.units[unit.index()].name()
    }

    fn binder(&self, binder: BinderRef) -> String {
        let unit = self.decls[binder.decl.index()].unit;
        let name = self.binders(binder.decl, binder.sig)[binder.slot].name;
        format!("binder {}", self.name(unit, name))
    }

    /// The judgments about what population interned
    fn populated(
        &self,
        db: &Database,
        unit: UnitId,
        judgments: &mut Vec<(&'static str, Span, String)>,
    ) {
        for (&(id, sig), &db_id) in &self.sig_decls {
            let decl = &self.decls[id.index()];
            if decl.unit != unit {
                continue;
            }
            let span = match &decl.node {
                DeclNode::Defs(defs) => defs[sig].name.span,
                DeclNode::Methods(methods) => methods[sig].name.span,
                _ => continue,
            };
            let declaration = db.declaration(db_id);
            let names = self.names(db, declaration);
            judgments.push(("decl", span, self.decl(db, declaration, &names)));
        }
        for (index, decl) in self.decls.iter().enumerate() {
            let id = DeclId::from_index(index);
            if decl.unit != unit {
                continue;
            }
            match &decl.node {
                DeclNode::Class(_) | DeclNode::Alias(_) => {
                    let declaration = db.declaration(id);
                    let names = self.names(db, declaration);
                    let span = decl.name_span().expect("a class or alias is named");
                    judgments.push(("decl", span, self.decl(db, declaration, &names)));
                }
                DeclNode::Defs(_) | DeclNode::Methods(_) | DeclNode::Closure(_) => {}
            }
        }
        for site in &self.sites {
            if site.unit != unit {
                continue;
            }
            let span = site.ty.span();
            let ty = site.interned.get().expect("population interns each site");
            let names = match site.group() {
                Some(key) => self.groups[&key]
                    .iter()
                    .map(|binder| self.binder_name(*binder))
                    .collect(),
                None => Vec::new(),
            };
            judgments.push(("type", span, self.render(db, ty, &names)));
        }
    }

    /// The name of a binder, or `in` or `out` for an implicit one
    fn binder_name(&self, binder: BinderRef) -> String {
        let unit = self.decls[binder.decl.index()].unit;
        match self.binders(binder.decl, binder.sig).get(binder.slot) {
            Some(written) => self.name(unit, written.name).to_owned(),
            None => match self.sigs[&(binder.decl, binder.sig)].input {
                Ambient::Implicit(input) if input == binder => "in".to_owned(),
                _ => "out".to_owned(),
            },
        }
    }

    /// The names of a declaration's outer group
    fn names(&self, db: &Database, declaration: &Declaration) -> Vec<String> {
        declaration
            .binders
            .iter()
            .map(|binder| match (binder.origin, db.symbol(binder.name)) {
                (BinderOrigin::Implicit, "<") => "in".to_owned(),
                (BinderOrigin::Implicit, _) => "out".to_owned(),
                (_, name) => name.to_owned(),
            })
            .collect()
    }

    /// A declaration: its outer group's slots, each with its origin, name,
    /// variance, bound and default, then a class's supertypes, an alias's definition,
    /// or a function's type
    fn decl(&self, db: &Database, declaration: &Declaration, names: &[String]) -> String {
        let mut out = String::new();
        let body = match db.ty(declaration.ty) {
            Type::Quantified { binders, body } => {
                let slots: Vec<_> = binders
                    .iter()
                    .zip(declaration.binders.iter())
                    .zip(names)
                    .map(|((binder, source), name)| {
                        let lifted = match source.origin {
                            BinderOrigin::Lifted => "^",
                            BinderOrigin::Written | BinderOrigin::Implicit => "",
                        };
                        let mut slot = format!("{lifted}{name}{}", variance(binder.variance));
                        if let Some(bound) = binder.bound {
                            let _ = write!(slot, " @ {}", self.render(db, bound, names));
                        }
                        if let Some(default) = binder.default {
                            let _ = write!(slot, " = {}", self.render(db, default, names));
                        }
                        slot
                    })
                    .collect();
                let _ = write!(out, "[{}] ", slots.join(", "));
                *body
            }
            _ => declaration.ty,
        };
        if declaration.source.kind.nominal() {
            out.push_str("nominal");
            let supertypes: Vec<_> = declaration
                .supertypes
                .iter()
                .map(|supertype| self.render(db, supertype.ty, names))
                .collect();
            if !supertypes.is_empty() {
                let _ = write!(out, " <: {}", supertypes.join(", "));
            }
        } else {
            out.push_str(&self.render(db, body, names));
        }
        out
    }

    /// A closed type
    #[cfg_attr(
        not(any(test, feature = "debug")),
        expect(dead_code, reason = "used by tests and traces")
    )]
    pub(crate) fn render_type(&self, db: &Database, ty: TypeId) -> String {
        db.render(ty, self, Style::Full)
    }

    /// A type as interned, with the binders of the group it is interpreted in named
    /// by `names`
    fn render(&self, db: &Database, ty: TypeId, names: &[String]) -> String {
        db.render_in(ty, names, self, Style::Full)
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
