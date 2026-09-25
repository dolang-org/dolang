//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use std::fmt::Write;

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, KindOf, ModuleRef, ParamTy, Referent, RestSlot,
    Sig, Slot, Tables,
};
use crate::{
    Mode, RestKind,
    ast::{Param, TypeExpr, visit::Node},
    source::Span,
    typeck::r#type::{DeclId, Kind, UnitId},
};

/// The judgments the tables record, by the name a fixture writes
pub(crate) const JUDGMENTS: &[&str] = &["ref", "head", "kind", "sig", "ambient", "designated"];

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
        for (binder, kind) in &self.binder_kinds {
            let owner = &self.decls[binder.decl.index()];
            if owner.unit == unit
                && let Some(written) = self.binders(binder.decl, binder.sig).get(binder.slot)
            {
                judgments.push(("kind", written.ident.span, self.kind(kind)));
            }
        }
        for (id, kind) in &self.alias_kinds {
            let decl = &self.decls[id.index()];
            if decl.unit == unit
                && let Some(name) = decl.name
            {
                judgments.push(("kind", name, self.kind(kind)));
            }
        }
        for (&(id, sig), completed) in &self.sigs {
            let decl = &self.decls[id.index()];
            if decl.unit == unit {
                let name = match decl.node {
                    DeclNode::Defs(ref defs) => defs[sig].ident.span,
                    DeclNode::Methods(ref methods) => methods[sig].name_span,
                    _ => unreachable!("only a def or method has a signature"),
                };
                judgments.push(("sig", name, self.sig(unit, completed)));
            }
        }
        for (arrow, ambients) in &self.func_ambients {
            if arrow.unit == unit {
                let [input, output] = ambients.map(|ambient| self.ambient(ambient));
                judgments.push(("ambient", arrow.span, format!("<{input} >{output}")));
            }
        }
        for (id, designated) in &self.designated {
            let decl = &self.decls[id.index()];
            if decl.unit == unit
                && let Some(name) = decl.name
            {
                let value = match designated {
                    Designated::Value => "top".to_owned(),
                    Designated::Intrinsic(intrinsic) => format!("intrinsic {intrinsic:?}"),
                };
                judgments.push(("designated", name, value));
            }
        }
        judgments.sort_by_key(|(name, span, _)| (span.start, span.end, *name));
        judgments
    }

    fn referent(&self, referent: &Referent) -> String {
        match referent {
            Referent::Decl(id) => self.qualified(*id),
            Referent::Binder(binder) => self.binder(*binder),
            Referent::External { module, item } => format!("external {module}.{item}"),
            Referent::Module(ModuleRef::Unit(unit)) => format!("module {}", self.unit_name(*unit)),
            Referent::Module(ModuleRef::External(module)) => format!("module {module}"),
            Referent::Value(_) => "value".to_owned(),
            Referent::Error => "error".to_owned(),
        }
    }

    fn kind(&self, kind: &KindOf) -> String {
        let name = match kind.kind {
            Kind::Type => "type",
            Kind::Schema => "schema",
        };
        match kind.flexible {
            true => format!("flexible {name}"),
            false => name.to_owned(),
        }
    }

    /// A completed signature, with annotations as written, omissions as the types
    /// they default to, and implicit binders by slot
    fn sig(&self, unit: UnitId, sig: &Sig<'_>) -> String {
        let text = |ty: &TypeExpr| self.text(unit, ty.span());
        let slot = |slot: &Slot<'_>| match slot {
            Slot::Annot(ty) => text(ty).to_owned(),
            Slot::Unknown => "Unknown".to_owned(),
            Slot::SelfType => "Self".to_owned(),
        };
        let mut out = String::from("(");
        for (index, (param, ty)) in sig.params.iter().enumerate() {
            if index != 0 {
                out.push_str(", ");
            }
            let (name, optional) = match param {
                Param::Pos { ident, default, .. } => {
                    (self.text(unit, ident.span).to_owned(), default.is_some())
                }
                Param::Key { ident, default, .. } | Param::ConstKey { ident, default, .. } => (
                    format!(":{}", self.text(unit, ident.span)),
                    default.is_some(),
                ),
                Param::Rest { kind, ident, .. } => {
                    let sigil = match kind {
                        RestKind::Mixed => "...",
                        RestKind::Pos => "*",
                        RestKind::Key => "**",
                    };
                    let name = ident
                        .as_ref()
                        .map_or("", |ident| self.text(unit, ident.span));
                    (format!("{sigil}{name}"), false)
                }
            };
            let ty = match ty {
                ParamTy::Single(single) => slot(single),
                ParamTy::Rest(RestSlot::Items(kind, item)) => {
                    let item = slot(item);
                    match kind {
                        RestKind::Mixed => format!("{{*{item}, **{item}}}"),
                        RestKind::Pos => format!("{{*{item}}}"),
                        RestKind::Key => format!("{{**{item}}}"),
                    }
                }
                ParamTy::Rest(RestSlot::Pack(ty)) => text(ty).to_owned(),
                ParamTy::Rest(RestSlot::Pattern(ty)) => format!("...{}", text(ty)),
            };
            let _ = write!(out, "{}{name}: {ty}", if optional { "?" } else { "" });
        }
        out.push(')');
        for (sigil, ambient) in [('<', sig.input), ('>', sig.output)] {
            match ambient {
                Ambient::Implicit(binder) => {
                    let _ = write!(out, " {sigil}#{}", binder.slot);
                }
                _ => {
                    let _ = write!(out, " {sigil}{}", self.ambient(ambient));
                }
            }
        }
        let _ = write!(out, " -> {}", slot(&sig.ret));
        out
    }

    fn ambient(&self, ambient: Ambient) -> String {
        match ambient {
            Ambient::Written => "written".to_owned(),
            Ambient::Implicit(binder) => {
                format!("{}#{}", self.qualified(binder.decl), binder.slot)
            }
            Ambient::Of(decl, _) => format!("of {}", self.qualified(decl)),
            Ambient::Unknown => "Unknown".to_owned(),
        }
    }

    fn head(&self, head: &Head) -> String {
        match head {
            Head::Decl(id) => self.qualified(*id),
            Head::Binder(binder) => self.binder(*binder),
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

    fn binder(&self, binder: BinderRef) -> String {
        let unit = self.decls[binder.decl.index()].unit;
        let ident = &self.binders(binder.decl, binder.sig)[binder.slot].ident;
        format!("binder {}", self.text(unit, ident.span))
    }
}
