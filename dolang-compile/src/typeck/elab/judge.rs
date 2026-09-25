//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use super::{DeclNode, Head, ModuleRef, Referent, Tables};
use crate::{
    Mode,
    source::Span,
    typeck::r#type::{DeclId, UnitId},
};

/// The judgments the tables record, by the name a fixture writes
pub(crate) const JUDGMENTS: &[&str] = &["ref", "head"];

impl Tables<'_> {
    /// Every judgment about spans of `unit`, in source order
    pub(crate) fn judgments(&self, unit: UnitId) -> Vec<(&'static str, Span, String)> {
        let mut judgments = Vec::new();
        for (head, referent) in &self.referents {
            if head.unit == unit {
                judgments.push(("ref", head.span, self.referent(referent)));
            }
        }
        for (id, head) in &self.aliases {
            let decl = &self.decls[id.index()];
            if decl.unit == unit
                && let Some(name) = decl.name
            {
                judgments.push(("head", name, self.head(head)));
            }
        }
        judgments.sort_by_key(|(name, span, _)| (span.start, span.end, *name));
        judgments
    }

    fn referent(&self, referent: &Referent) -> String {
        match referent {
            Referent::Decl(id) => self.qualified(*id),
            Referent::Binder { decl, sig, slot } => self.binder(*decl, *sig, *slot),
            Referent::External { module, item } => format!("external {module}.{item}"),
            Referent::Module(ModuleRef::Unit(unit)) => format!("module {}", self.unit_name(*unit)),
            Referent::Module(ModuleRef::External(module)) => format!("module {module}"),
            Referent::Value(_) => "value".to_owned(),
            Referent::Error => "error".to_owned(),
        }
    }

    fn head(&self, head: &Head) -> String {
        match head {
            Head::Decl(id) => self.qualified(*id),
            Head::Binder { decl, sig, slot } => self.binder(*decl, *sig, *slot),
            Head::External { module, item } => format!("external {module}.{item}"),
            Head::Structural => "structural".to_owned(),
            Head::Error => "error".to_owned(),
        }
    }

    /// A declaration's name, qualified by its unit and the declarations it is
    /// nested in
    fn qualified(&self, id: DeclId) -> String {
        let decl = &self.decls[id.index()];
        let mut name = match decl.outer {
            Some(outer) => self.qualified(outer),
            None => self.unit_name(decl.unit),
        };
        name.push('.');
        match decl.name {
            Some(span) => name.push_str(self.units[decl.unit.index()].compiler.file.str(span)),
            None => name.push_str("<closure>"),
        }
        name
    }

    /// A module's name, or a script's file stem
    fn unit_name(&self, unit: UnitId) -> String {
        let compiler = &self.units[unit.index()].compiler;
        match compiler.mode {
            Mode::Module { name } => name.to_owned(),
            Mode::Script | Mode::Repl => compiler
                .file
                .path()
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned()),
        }
    }

    fn binder(&self, decl: DeclId, sig: usize, slot: usize) -> String {
        let owner = &self.decls[decl.index()];
        let binders = match &owner.node {
            DeclNode::Class(class) => class.binders.as_deref(),
            DeclNode::Alias(alias) => alias.binders.as_deref(),
            DeclNode::Defs(defs) => defs[sig].binders.as_deref(),
            DeclNode::Methods(methods) => methods[sig].binders.as_deref(),
            DeclNode::Closure(_) => None,
        };
        let binder = &binders.expect("a binder's declaration has binders").binders[slot];
        let file = &self.units[owner.unit.index()].compiler.file;
        format!("binder {}", file.str(binder.ident.span))
    }
}
