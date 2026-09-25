//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use std::{collections::HashMap, fmt::Write};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, KindOf, ModuleRef, ParamTy, Referent, RestSlot,
    Sig, Slot, Tables,
};
use crate::{
    Mode, RestKind,
    ast::{ClassMember, Param, TypeExpr, visit::Node},
    source::Span,
    typeck::r#type::{
        Argument, BinderOrigin, Database, DeclId, Declaration, Element, Kind, Literal, Member,
        Multiplicity, Scope, Type, TypeId, UnionMember, UnitId, UnitSpan, Variance,
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
];

fn variance(variance: Variance) -> &'static str {
    match variance {
        Variance::Covariant => "covariant",
        Variance::Contravariant => "contravariant",
        Variance::Invariant => "invariant",
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
                judgments.push(("variance", written.ident.span, variance(value).to_owned()));
            }
        }
        let mut captured: HashMap<DeclId, Vec<(BinderRef, Variance)>> = HashMap::new();
        for (&(id, binder), &value) in &self.captured {
            if self.decls[id.index()].unit == unit {
                captured.entry(id).or_default().push((binder, value));
            }
        }
        for (id, mut binders) in captured {
            let Some(name) = self.decls[id.index()].name else {
                continue;
            };
            // Outer declarations are allocated first
            binders.sort_by_key(|(binder, _)| (binder.decl, binder.sig, binder.slot));
            let value = binders
                .iter()
                .map(|&(binder, value)| {
                    let unit = self.decls[binder.decl.index()].unit;
                    let ident = &self.binders(binder.decl, binder.sig)[binder.slot].ident;
                    format!("{} {}", self.text(unit, ident.span), variance(value))
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
                && let Some(name) = decl.name
            {
                let value = match designated {
                    Designated::Value => "top".to_owned(),
                    Designated::Phantom => "phantom".to_owned(),
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
            Some((outer, _)) => self.qualified(outer),
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
                DeclNode::Defs(defs) => defs[sig].ident.span,
                DeclNode::Methods(methods) => methods[sig].name_span,
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
            match decl.node {
                DeclNode::Class(class) => {
                    let declaration = db.declaration(id);
                    let names = self.names(db, declaration);
                    let span = decl.name.expect("a class is named");
                    judgments.push(("quantifier", span, self.quantifier(db, declaration, &names)));
                    judgments.push(("decl", span, self.decl(db, declaration, &names)));
                    let mut spans = Vec::new();
                    for member in &class.body.members {
                        match member {
                            ClassMember::Field(field) => {
                                spans.extend(field.fields.iter().map(|name| name.ident.span))
                            }
                            ClassMember::Method(method) => spans.push(method.name_span),
                        }
                    }
                    for (key, member) in declaration.members.iter() {
                        // The first member of a name is the one recorded
                        let Some(span) = spans
                            .iter()
                            .copied()
                            .find(|&span| self.text(unit, span) == db.symbol(key.name))
                        else {
                            continue;
                        };
                        judgments.push(("member", span, self.member(db, member, &names)));
                    }
                }
                DeclNode::Alias(_) => {
                    let declaration = db.declaration(id);
                    let names = self.names(db, declaration);
                    let span = decl.name.expect("an alias is named");
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
            Some(written) => self.text(unit, written.ident.span).to_owned(),
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
                .map(|&ty| self.render(db, ty, names))
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
        let (what, scope, public) = match member {
            Member::Field { scope, public, .. } => ("field", scope, public),
            Member::Method { scope, public, .. } => ("method", scope, public),
        };
        let scope = match scope {
            Scope::Instance => "instance",
            Scope::Class => "class",
            Scope::Static => "static",
        };
        let visibility = if *public { "pub" } else { "private" };
        let mut out = format!("{what} {scope} {visibility}");
        match member {
            Member::Field { ty, .. } => {
                let _ = write!(out, " {}", self.render(db, *ty, names));
            }
            Member::Method { decl, .. } => {
                let _ = write!(out, " {}", self.qualified(*decl));
            }
        }
        out
    }

    /// A type as interned, with the binders of the group it is interpreted in named
    /// by `names`
    fn render(&self, db: &Database, ty: TypeId, names: &[String]) -> String {
        let mut out = String::new();
        self.render_into(db, ty, names, 0, &mut out);
        out
    }

    fn render_into(
        &self,
        db: &Database,
        ty: TypeId,
        names: &[String],
        depth: u16,
        out: &mut String,
    ) {
        match db.ty(ty) {
            Type::Top => out.push_str("Value"),
            Type::Unknown(Kind::Type) => out.push_str("Unknown"),
            Type::Unknown(Kind::Schema) => out.push_str("Unknown{}"),
            Type::Literal(literal) => {
                let _ = match literal {
                    Literal::Nil => write!(out, "nil"),
                    Literal::Bool(value) => write!(out, "{value}"),
                    Literal::Int(value) => write!(out, "{value}"),
                    Literal::Str(value) => write!(out, "{value:?}"),
                    Literal::Sym(sym) => write!(out, ":{}:", db.symbol(*sym)),
                };
            }
            Type::Decl(id) => out.push_str(&self.qualified(*id)),
            Type::Rigid { decl, slot, .. } => {
                let _ = write!(out, "{}.#{slot}", self.qualified(*decl));
            }
            Type::Bound { reference, .. } => match names.get(usize::from(reference.slot)) {
                Some(name) if reference.depth == depth => out.push_str(name),
                _ => {
                    let _ = write!(out, "#{}.{}", reference.depth, reference.slot);
                }
            },
            Type::Apply { base, args, .. } => {
                self.render_into(db, *base, names, depth, out);
                // Lifted arguments are marked
                let lifted = match db.ty(*base) {
                    Type::Decl(id) => db
                        .declaration(*id)
                        .binders
                        .iter()
                        .take_while(|binder| binder.origin == BinderOrigin::Lifted)
                        .count(),
                    _ => 0,
                };
                out.push('[');
                for (index, arg) in args.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    if index < lifted {
                        out.push('^');
                    }
                    match arg {
                        Argument::Positional(ty) => self.render_into(db, *ty, names, depth, out),
                        Argument::Keyword(name, ty) => {
                            let _ = write!(out, "{}: ", db.symbol(*name));
                            self.render_into(db, *ty, names, depth, out);
                        }
                        Argument::Expand(ty) => {
                            out.push_str("...");
                            self.render_into(db, *ty, names, depth, out);
                        }
                    }
                }
                out.push(']');
            }
            Type::Union(members) => {
                if members.is_empty() {
                    out.push_str("Never");
                }
                for (index, member) in members.iter().enumerate() {
                    if index != 0 {
                        out.push_str(" | ");
                    }
                    match member {
                        UnionMember::Type(ty) => self.render_into(db, *ty, names, depth, out),
                        UnionMember::Expand(ty) => {
                            out.push_str("...");
                            self.render_into(db, *ty, names, depth, out);
                        }
                    }
                }
            }
            Type::Function(func) => {
                out.push('(');
                self.items(db, func.params, names, depth, out);
                out.push(')');
                for (sigil, channel) in [('<', func.input), ('>', func.output)] {
                    if let Some(channel) = channel {
                        let _ = write!(out, " {sigil}");
                        self.render_into(db, channel, names, depth, out);
                    }
                }
                out.push_str(" -> ");
                self.render_into(db, func.result, names, depth, out);
            }
            Type::Schema(_) => {
                out.push('{');
                self.items(db, ty, names, depth, out);
                out.push('}');
            }
            Type::Quantified { body, .. } => {
                out.push_str("forall ");
                self.render_into(db, *body, names, depth + 1, out);
            }
        }
    }

    /// The items of a schema, or what stands for one
    fn items(&self, db: &Database, ty: TypeId, names: &[String], depth: u16, out: &mut String) {
        let Type::Schema(items) = db.ty(ty) else {
            out.push_str("...");
            return self.render_into(db, ty, names, depth, out);
        };
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                out.push_str(", ");
            }
            out.push_str(match item.multiplicity {
                Multiplicity::Required => "",
                Multiplicity::Optional => "?",
                Multiplicity::Repeated => "*",
            });
            match item.element {
                Element::Positional(ty) => self.render_into(db, ty, names, depth, out),
                Element::Keyed { key, value } => {
                    match db.ty(key) {
                        Type::Literal(Literal::Sym(sym)) => out.push_str(db.symbol(*sym)),
                        _ => {
                            out.push('(');
                            self.render_into(db, key, names, depth, out);
                            out.push(')');
                        }
                    }
                    out.push_str(": ");
                    self.render_into(db, value, names, depth, out);
                }
                Element::Include(ty) => {
                    out.push_str("...");
                    self.render_into(db, ty, names, depth, out);
                }
            }
        }
    }
}
