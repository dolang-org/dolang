//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use std::{borrow::Cow, collections::HashMap, fmt::Write};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, KindOf, ModuleRef, ParamTy, Referent, RestSlot,
    Slot, Tables, Unresolved,
    surface::{Member as SourceMember, MemberScope, ParamKind},
};
use crate::{
    RestKind,
    source::Span,
    typeck::r#type::{
        BinderOrigin, Collection, Database, DeclId, Declaration, Intrinsic, Kind, Member, Names,
        Scope, Style, Type, TypeId, UnitId, UnitSpan, Variance,
    },
};

/// The judgments the tables record, by the name a fixture writes
pub(crate) const JUDGMENTS: &[&str] = &[
    "ref",
    "head",
    "kind",
    "sig",
    "ambient",
    "designated",
    "variance",
    "captured",
    "quantifier",
    "decl",
    "member",
    "type",
    "wf",
    "flow",
];

fn variance(variance: Variance) -> &'static str {
    match variance {
        Variance::Covariant => "covariant",
        Variance::Contravariant => "contravariant",
        Variance::Invariant => "invariant",
    }
}

impl Tables<'_> {
    /// Every judgment about spans of `unit`, in source order, including the
    /// well-formedness checks left unresolved
    pub(crate) fn judgments(
        &self,
        db: &Database,
        unit: UnitId,
        unresolved: &[Unresolved],
    ) -> Vec<(&'static str, Span, String)> {
        let mut judgments = Vec::new();
        self.populated(db, unit, &mut judgments);
        for (head, referent) in &self.referents {
            if head.unit == unit {
                judgments.push(("ref", head.span, self.referent(referent)));
            }
        }
        for (id, head) in &self.aliases {
            let decl = &self.decls[id.index()];
            if decl.unit == unit
                && let Some(name) = decl.name_span()
            {
                judgments.push(("head", name, self.head(head)));
            }
        }
        for (binder, kind) in &self.binder_kinds {
            let owner = &self.decls[binder.decl.index()];
            if owner.unit == unit
                && let Some(written) = self.binders(binder.decl, binder.sig).get(binder.slot)
            {
                judgments.push(("kind", written.name.span, self.kind(kind)));
            }
        }
        for (id, kind) in &self.alias_kinds {
            let decl = &self.decls[id.index()];
            if decl.unit == unit
                && let Some(name) = decl.name_span()
            {
                judgments.push(("kind", name, self.kind(kind)));
            }
        }
        for (&(id, sig), completed) in &self.sigs {
            let decl = &self.decls[id.index()];
            if decl.unit == unit {
                let name = match &decl.node {
                    DeclNode::Defs(defs) => defs[sig].name.span,
                    DeclNode::Methods(methods) => methods[sig].name.span,
                    _ => unreachable!("only a def or method has a signature"),
                };
                judgments.push(("sig", name, self.sig(id, sig)));
                let implicit: Vec<_> = [('<', completed.input), ('>', completed.output)]
                    .into_iter()
                    .filter_map(|(sigil, ambient)| match ambient {
                        Ambient::Implicit(binder) => Some(format!(
                            "{sigil}#{} {}",
                            binder.slot,
                            variance(self.variance[&binder])
                        )),
                        _ => None,
                    })
                    .collect();
                if !implicit.is_empty() {
                    judgments.push(("variance", name, implicit.join(" ")));
                }
            }
        }
        for (binder, &value) in &self.variance {
            let owner = &self.decls[binder.decl.index()];
            if owner.unit == unit
                && let Some(written) = self.binders(binder.decl, binder.sig).get(binder.slot)
            {
                judgments.push(("variance", written.name.span, variance(value).to_owned()));
            }
        }
        let mut captured: HashMap<DeclId, Vec<(BinderRef, Variance)>> = HashMap::new();
        for (&(id, binder), &value) in &self.captured {
            if self.decls[id.index()].unit == unit {
                captured.entry(id).or_default().push((binder, value));
            }
        }
        for (id, mut binders) in captured {
            let Some(name) = self.decls[id.index()].name_span() else {
                continue;
            };
            // Outer declarations are allocated first
            binders.sort_by_key(|(binder, _)| (binder.decl, binder.sig, binder.slot));
            let value = binders
                .iter()
                .map(|&(binder, value)| {
                    let unit = self.decls[binder.decl.index()].unit;
                    let name = self.binders(binder.decl, binder.sig)[binder.slot].name;
                    format!("{} {}", self.name(unit, name), variance(value))
                })
                .collect::<Vec<_>>()
                .join(", ");
            judgments.push(("captured", name, value));
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
                && let Some(name) = decl.name_span()
            {
                let value = match designated {
                    Designated::Value => "top".to_owned(),
                    Designated::Never => "bottom".to_owned(),
                    Designated::Phantom => "phantom".to_owned(),
                    Designated::Getter => "getter".to_owned(),
                    Designated::Setter => "setter".to_owned(),
                    Designated::Fmt => "fmt".to_owned(),
                    Designated::FmtValue => "fmt value".to_owned(),
                    Designated::FmtParam => "fmt param".to_owned(),
                    Designated::Float => "float".to_owned(),
                    Designated::Bin => "bin".to_owned(),
                    Designated::Array => "array".to_owned(),
                    Designated::Dict => "dict".to_owned(),
                    Designated::Record => "record".to_owned(),
                    Designated::Range => "range".to_owned(),
                    Designated::Spread => "spread".to_owned(),
                    Designated::Unpack => "unpack".to_owned(),
                    Designated::PipeSender => "pipe sender".to_owned(),
                    Designated::PipeReceiver => "pipe receiver".to_owned(),
                    Designated::Intrinsic(intrinsic) => format!("intrinsic {intrinsic:?}"),
                };
                judgments.push(("designated", name, value));
            }
        }
        for unresolved in unresolved {
            if unresolved.span.unit == unit {
                let value = format!("undecided {:?}", unresolved.residual);
                judgments.push(("wf", unresolved.span.span, value));
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

    /// A completed signature, with annotations in canonical form, omissions as the
    /// types they default to, and implicit binders by slot
    fn sig(&self, decl: DeclId, sig: usize) -> String {
        let unit = self.decls[decl.index()].unit;
        let func = self.decls[decl.index()].node.signature(sig);
        let sig = &self.sigs[&(decl, sig)];
        let text = |site| self.print(unit, self.site_ty(site));
        let slot = |slot: &Slot| match *slot {
            Slot::Annot(ty) => text(ty),
            Slot::Unknown => "Unknown".to_owned(),
            Slot::SelfType => "Self".to_owned(),
            Slot::Nil => "nil".to_owned(),
        };
        let mut out = String::from("(");
        for (index, (param, ty)) in func.params.iter().zip(&sig.params).enumerate() {
            if index != 0 {
                out.push_str(", ");
            }
            // Only a rest or a sub-pattern is nameless
            let spelled = match (param.name, &param.kind) {
                (Some(name), _) => self.name(unit, name),
                (None, ParamKind::Rest { .. }) => "",
                (None, _) => "()",
            };
            let (name, optional) = match param.kind {
                ParamKind::Pos => (spelled.to_owned(), param.default),
                ParamKind::Key { .. } | ParamKind::ConstKey { .. } => {
                    (format!(":{spelled}"), param.default)
                }
                ParamKind::Rest { kind, .. } => {
                    let sigil = match kind {
                        RestKind::Mixed => "...",
                        RestKind::Pos => "*",
                        RestKind::Key => "**",
                    };
                    (format!("{sigil}{spelled}"), false)
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
                ParamTy::Rest(RestSlot::Pack(ty)) => text(*ty),
                ParamTy::Rest(RestSlot::Pattern(ty)) => format!("...{}", text(*ty)),
            };
            let _ = write!(out, "{}{name}: {ty}", if optional { "?" } else { "" });
        }
        for (sigil, ambient) in [('<', sig.input), ('>', sig.output)] {
            if !out.ends_with('(') {
                out.push_str(", ");
            }
            match ambient {
                Ambient::Implicit(binder) => {
                    let _ = write!(out, "{sigil}#{}", binder.slot);
                }
                _ => {
                    let _ = write!(out, "{sigil}{}", self.ambient(ambient));
                }
            }
        }
        out.push(')');
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
            Ambient::Value => "Value".to_owned(),
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
        let sig = (self.sig_decls.iter()).find(|&(_, &declaration)| declaration == id);
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
            judgments.push(("quantifier", span, self.quantifier(db, declaration, &names)));
            judgments.push(("decl", span, self.decl(db, declaration, &names)));
        }
        for (index, decl) in self.decls.iter().enumerate() {
            let id = DeclId::from_index(index);
            if decl.unit != unit {
                continue;
            }
            match &decl.node {
                DeclNode::Class(class) => {
                    let declaration = db.declaration(id);
                    let names = self.names(db, declaration);
                    let span = decl.name_span().expect("a class is named");
                    judgments.push(("quantifier", span, self.quantifier(db, declaration, &names)));
                    judgments.push(("decl", span, self.decl(db, declaration, &names)));
                    for (key, member) in declaration.members.iter() {
                        // A method member is judged at its function's name, and a field at
                        // the first field its key and namespace record
                        let span = match member.decls().next() {
                            Some(decl) => self.decls[decl.index()].name_span(),
                            None => class.members.iter().find_map(|source| {
                                let SourceMember::Field(field) = source else {
                                    return None;
                                };
                                let instance = field.scope == MemberScope::Instance;
                                field
                                    .names
                                    .iter()
                                    .find(|&&name| {
                                        self.name(unit, name) == db.symbol(key.name)
                                            && !field.public == key.private
                                            && instance == (member.scope() == Scope::Instance)
                                    })
                                    .map(|name| name.span)
                            }),
                        };
                        if let Some(span) = span {
                            judgments.push(("member", span, self.member(db, member, &names)));
                        }
                    }
                }
                DeclNode::Alias(_) => {
                    let declaration = db.declaration(id);
                    let names = self.names(db, declaration);
                    let span = decl.name_span().expect("an alias is named");
                    judgments.push(("quantifier", span, self.quantifier(db, declaration, &names)));
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
            let ty = self.site_types[&UnitSpan { unit, span }];
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

    /// A declaration's outer group: each slot's origin, name, bound and variance
    fn quantifier(&self, db: &Database, declaration: &Declaration, names: &[String]) -> String {
        let Type::Quantified { binders, .. } = db.ty(declaration.ty) else {
            return "none".to_owned();
        };
        binders
            .iter()
            .zip(declaration.binders.iter())
            .zip(names)
            .map(|((binder, source), name)| {
                let mut out = match source.origin {
                    BinderOrigin::Lifted => format!("^{name}"),
                    BinderOrigin::Written | BinderOrigin::Implicit => name.clone(),
                };
                if let Some(bound) = binder.bound {
                    let _ = write!(out, " @ {}", self.render(db, bound, names));
                }
                if let Some(default) = binder.default {
                    let _ = write!(out, " = {}", self.render(db, default, names));
                }
                let _ = write!(out, " {}", variance(binder.variance));
                out
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A declaration's body: a class's supertypes, an alias's definition, or a
    /// function's type
    fn decl(&self, db: &Database, declaration: &Declaration, names: &[String]) -> String {
        let body = match db.ty(declaration.ty) {
            Type::Quantified { body, .. } => *body,
            _ => declaration.ty,
        };
        if declaration.source.kind.nominal() {
            let supertypes: Vec<_> = declaration
                .supertypes
                .iter()
                .map(|supertype| self.render(db, supertype.ty, names))
                .collect();
            match supertypes.is_empty() {
                true => "nominal".to_owned(),
                false => format!("nominal <: {}", supertypes.join(", ")),
            }
        } else {
            self.render(db, body, names)
        }
    }

    fn member(&self, db: &Database, member: &Member, names: &[String]) -> String {
        let what = match member {
            Member::Field { .. } => "field",
            Member::Method { .. } => "method",
            Member::Property { .. } => "property",
            Member::Decorated { .. } => "decorated",
        };
        let scope = match member.scope() {
            Scope::Instance => "instance",
            Scope::Class => "class",
            Scope::Static => "static",
        };
        let visibility = if member.public() { "pub" } else { "private" };
        let mut out = format!("{what} {scope} {visibility}");
        match member {
            Member::Field { ty, .. } => {
                let _ = write!(out, " {}", self.render(db, *ty, names));
            }
            Member::Method { decl, .. } | Member::Decorated { decl, .. } => {
                let _ = write!(out, " {}", self.qualified(*decl));
            }
            Member::Property { getter, setter, .. } => {
                for (what, decl) in [("get", getter), ("set", setter)] {
                    if let Some(decl) = decl {
                        let _ = write!(out, " {what} {}", self.qualified(*decl));
                    }
                }
            }
        }
        out
    }

    /// A closed type
    pub(crate) fn render_type(&self, db: &Database, ty: TypeId) -> String {
        db.render(ty, self, Style::Full)
    }

    /// A signature of the function `decl`, `ty`, as a diagnostic shows it: its
    /// own written binders named and listed before it, as in `[K] (K) -> K`, and
    /// the ambient channels its declaration leaves implicit left out. Binders
    /// `ty` holds before its own, as a class's in a constructor, are shown as
    /// references.
    pub(crate) fn render_signature(&self, db: &Database, decl: DeclId, ty: TypeId) -> String {
        let Type::Quantified { binders, body } = db.ty(ty) else {
            return self.render_type(db, ty);
        };
        let Type::Function(function) = db.ty(*body) else {
            return self.render_type(db, ty);
        };
        let declaration = db.declaration(decl);
        let own: Vec<(BinderOrigin, String)> = (declaration.binders.iter())
            .zip(self.names(db, declaration))
            .filter(|(binder, _)| binder.origin != BinderOrigin::Lifted)
            .map(|(binder, name)| (binder.origin, name))
            .collect();
        let Some(offset) = binders.len().checked_sub(own.len()) else {
            return self.render_type(db, ty);
        };
        let names: Vec<String> = (0..offset)
            .map(|slot| format!("#0.{slot}"))
            .chain(own.iter().map(|(_, name)| name.clone()))
            .collect();
        let implicit = |slot: u16| {
            let slot = usize::from(slot);
            slot >= offset && own[slot - offset].0 == BinderOrigin::Implicit
        };
        let shown = db.without_channels(db.intern(Type::Function(function.clone())), &implicit, 0);
        let written: Vec<&str> = (own.iter())
            .filter(|(origin, _)| *origin == BinderOrigin::Written)
            .map(|(_, name)| name.as_str())
            .collect();
        let rendered = self.render(db, shown, &names);
        match written[..] {
            [] => rendered,
            _ => format!("@[{}] {rendered}", written.join(", ")),
        }
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
