//! Judgments: the tables' facts about source spans, formatted for regression tests.
//!
//! A judgment names what the checker concluded at a span in terms a fixture can
//! write down: qualified names rather than IDs.

use std::{collections::HashMap, convert::Infallible, fmt::Write};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, KindOf, ModuleRef, ParamTy, Referent, RestSlot,
    Slot, Tables, Unresolved,
    surface::{Member as SourceMember, MemberScope, ParamKind},
};
use crate::{
    RestKind,
    source::Span,
    typeck::r#type::{
        Argument, Binder, BinderOrigin, Binding, BoundRef, Database, DeclId, Declaration, Element,
        Kind, Literal, Member, Multiplicity, Scope, Type, TypeId, UnionMember, UnitId, UnitSpan,
        Variance,
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
                    Designated::BaseIterable => "base iterable".to_owned(),
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
            Slot::Top => "Value".to_owned(),
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
            Ambient::Unknown => "Unknown".to_owned(),
            Ambient::Strict => "strict".to_owned(),
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
        self.render(db, ty, &[])
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
        let shown = without_channels(
            db,
            db.intern(Type::Function(function.clone())),
            &implicit,
            0,
        );
        let written: Vec<&str> = (own.iter())
            .filter(|(origin, _)| *origin == BinderOrigin::Written)
            .map(|(_, name)| name.as_str())
            .collect();
        let rendered = self.render(db, shown, &names);
        match written[..] {
            [] => rendered,
            _ => format!("[{}] {rendered}", written.join(", ")),
        }
    }

    /// A type as interned, with the binders of the group it is interpreted in named
    /// by `names`
    fn render(&self, db: &Database, ty: TypeId, names: &[String]) -> String {
        let mut out = String::new();
        let naming = Naming {
            names,
            depth: 0,
            pattern: None,
        };
        self.render_into(db, ty, naming, &mut out);
        out
    }

    fn render_into(&self, db: &Database, ty: TypeId, naming: Naming<'_>, out: &mut String) {
        match db.ty(ty) {
            Type::Top => out.push_str("Value"),
            Type::Unknown(Kind::Type) => out.push_str("Unknown"),
            Type::Unknown(Kind::Schema) => out.push_str("Unknown{}"),
            Type::Unsupported {
                kind: Kind::Type, ..
            } => out.push_str("Unsupported"),
            Type::Unsupported {
                kind: Kind::Schema, ..
            } => out.push_str("Unsupported{}"),
            // Freshness is the checker's concern, not the reader's
            Type::Literal(literal) | Type::Fresh(literal) => {
                let _ = match literal {
                    Literal::Nil => write!(out, "nil"),
                    Literal::Bool(value) => write!(out, "{value}"),
                    Literal::Int(value) => write!(out, "{value}"),
                    Literal::Str(value) => write!(out, "{value:?}"),
                    Literal::Sym(sym) => write!(out, ":{}:", db.symbol(*sym)),
                };
            }
            Type::Decl(id) => out.push_str(&self.declared(*id)),
            Type::Rigid { decl, slot, .. } => {
                let _ = write!(out, "{}.#{slot}", self.declared(*decl));
            }
            Type::Bound { reference, .. } => match naming.name(*reference) {
                Some(name) => out.push_str(name),
                None => {
                    let _ = write!(out, "#{}.{}", reference.depth, reference.slot);
                }
            },
            Type::Apply { base, args, .. } => {
                self.render_into(db, *base, naming, out);
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
                let binders: &[Binder] = match db.ty(*base) {
                    Type::Decl(id) => match db.ty(db.declaration(*id).ty) {
                        Type::Quantified { binders, .. } if binders.len() == args.len() => binders,
                        _ => &[],
                    },
                    _ => &[],
                };
                // A keyword binder's argument is named, and trailing arguments equal
                // to their binders' defaults are left out
                let mut shown = args.len();
                if let Some(given) = (args.iter())
                    .map(|arg| match *arg {
                        Argument::Positional(ty) => Some(ty),
                        _ => None,
                    })
                    .collect::<Option<Vec<TypeId>>>()
                {
                    while shown > lifted
                        && let Some(default) = binders.get(shown - 1).and_then(|b| b.default)
                        && db.substitute(default, &given) == given[shown - 1]
                    {
                        shown -= 1;
                    }
                }
                out.push('[');
                for (index, arg) in args.iter().enumerate().take(shown) {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    if index < lifted {
                        out.push('^');
                    }
                    match arg {
                        Argument::Positional(ty) => {
                            if let Some(Binding::Keyword(name)) =
                                binders.get(index).map(|binder| binder.binding)
                            {
                                let _ = write!(out, "{}: ", db.symbol(name));
                            }
                            self.render_into(db, *ty, naming, out)
                        }
                        Argument::Keyword(name, ty) => {
                            let _ = write!(out, "{}: ", db.symbol(*name));
                            self.render_into(db, *ty, naming, out);
                        }
                        Argument::Expand(ty) => {
                            out.push_str("...");
                            self.render_into(db, *ty, naming, out);
                        }
                    }
                }
                out.push(']');
            }
            // Its signatures, any of which it's called as
            Type::Overloaded { overloads, .. } => {
                for (index, &overload) in overloads.iter().enumerate() {
                    if index != 0 {
                        out.push_str(" & ");
                    }
                    out.push('(');
                    self.render_into(db, overload, naming, out);
                    out.push(')');
                }
            }
            Type::Union(members) => {
                if members.is_empty() {
                    out.push_str("Never");
                }
                // Members are interned in ID order, which isn't stable across runs
                let mut rendered: Vec<String> = (members.iter())
                    .map(|member| {
                        let mut out = String::new();
                        match member {
                            // A function's result would take the members after it
                            UnionMember::Type(ty)
                                if matches!(
                                    db.ty(*ty),
                                    Type::Function(_) | Type::Quantified { .. }
                                ) =>
                            {
                                out.push('(');
                                self.render_into(db, *ty, naming, &mut out);
                                out.push(')');
                            }
                            UnionMember::Type(ty) => self.render_into(db, *ty, naming, &mut out),
                            UnionMember::Expand(ty) => {
                                out.push_str("...");
                                self.render_into(db, *ty, naming, &mut out);
                            }
                            UnionMember::Keys(ty)
                            | UnionMember::Values(ty)
                            | UnionMember::Entries(ty) => {
                                let name = match member {
                                    UnionMember::Keys(_) => "Keys",
                                    UnionMember::Values(_) => "Values",
                                    _ => "Entries",
                                };
                                let _ = write!(out, "{name}[...");
                                self.render_into(db, *ty, naming, &mut out);
                                out.push(']');
                            }
                            UnionMember::IndexItem(schema, key)
                            | UnionMember::AssignItem(schema, key) => {
                                let name = match member {
                                    UnionMember::IndexItem(..) => "IndexItem",
                                    _ => "AssignItem",
                                };
                                let _ = write!(out, "{name}[");
                                self.render_into(db, *schema, naming, &mut out);
                                out.push_str(", ");
                                self.render_into(db, *key, naming, &mut out);
                                out.push(']');
                            }
                        }
                        out
                    })
                    .collect();
                rendered.sort();
                out.push_str(&rendered.join(" | "));
            }
            Type::Function(func) => {
                out.push('(');
                self.items(db, func.params, naming, out);
                for (sigil, channel) in [('<', func.input), ('>', func.output)] {
                    if let Some(channel) = channel {
                        if !out.ends_with('(') {
                            out.push_str(", ");
                        }
                        out.push(sigil);
                        self.render_into(db, channel, naming, out);
                    }
                }
                out.push(')');
                out.push_str(" -> ");
                self.render_into(db, func.result, naming, out);
            }
            Type::Schema(_) => {
                out.push('{');
                self.items(db, ty, naming, out);
                out.push('}');
            }
            Type::Quantified { body, .. } => {
                out.push_str("forall ");
                self.render_into(db, *body, naming.enter(), out);
            }
            Type::Map { packs, pattern } => {
                out.push_str("{...");
                self.pattern(db, packs, *pattern, naming, out);
                out.push('}');
            }
        }
    }

    /// A mapping's pattern, as written: each item is named by its pack
    fn pattern(
        &self,
        db: &Database,
        packs: &[TypeId],
        pattern: TypeId,
        naming: Naming<'_>,
        out: &mut String,
    ) {
        let items = Pattern {
            depth: naming.depth + 1,
            packs: (packs.iter())
                .map(|&pack| {
                    let mut out = String::new();
                    self.render_into(db, pack, naming, &mut out);
                    out
                })
                .collect(),
            outer: naming.pattern,
        };
        let naming = Naming {
            pattern: Some(&items),
            ..naming.enter()
        };
        self.render_into(db, pattern, naming, out);
    }

    /// The items of a schema, or what stands for one
    fn items(&self, db: &Database, ty: TypeId, naming: Naming<'_>, out: &mut String) {
        let Type::Schema(items) = db.ty(ty) else {
            out.push_str("...");
            return self.render_into(db, ty, naming, out);
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
                Element::Positional(ty) => self.render_into(db, ty, naming, out),
                Element::Keyed { key, value } => {
                    match db.ty(key) {
                        Type::Literal(Literal::Sym(sym)) => out.push_str(db.symbol(*sym)),
                        _ => {
                            out.push('(');
                            self.render_into(db, key, naming, out);
                            out.push(')');
                        }
                    }
                    out.push_str(": ");
                    self.render_into(db, value, naming, out);
                }
                Element::Include(ty) => {
                    out.push_str("...");
                    match db.ty(ty) {
                        Type::Map { packs, pattern } => {
                            self.pattern(db, packs, *pattern, naming, out)
                        }
                        _ => self.render_into(db, ty, naming, out),
                    }
                }
            }
        }
    }
}

/// How the references of a rendered type are named
#[derive(Clone, Copy)]
struct Naming<'a> {
    /// The binders of the group the rendered type is interpreted in
    names: &'a [String],
    /// How many groups have been entered since
    depth: u16,
    /// The innermost mapping whose pattern is being rendered
    pattern: Option<&'a Pattern<'a>>,
}

/// `ty` with each function's channel left out where it's an implicit binder of
/// the group `depth` groups out, as `implicit` says of its slot
fn without_channels(
    db: &Database,
    ty: TypeId,
    implicit: &dyn Fn(u16) -> bool,
    depth: u32,
) -> TypeId {
    let mapped = db.ty(ty).map_children(|child, groups| {
        Ok::<_, Infallible>(without_channels(db, child, implicit, depth + groups))
    });
    let Ok(mut node) = mapped;
    if let Type::Function(function) = &mut node {
        for channel in [&mut function.input, &mut function.output] {
            if let Some(ty) = *channel
                && let Type::Bound { reference, .. } = *db.ty(ty)
                && u32::from(reference.depth) == depth
                && implicit(reference.slot)
            {
                *channel = None;
            }
        }
    }
    db.intern(node)
}

/// A mapping whose pattern is being rendered
struct Pattern<'a> {
    /// The depth of the pattern's group
    depth: u16,
    /// Each pack, which names its items
    packs: Vec<String>,
    outer: Option<&'a Pattern<'a>>,
}

impl Naming<'_> {
    fn enter(self) -> Self {
        Self {
            depth: self.depth + 1,
            ..self
        }
    }

    /// The name of a reference to the rendered type's group or a pattern's
    fn name(&self, reference: BoundRef) -> Option<&str> {
        let level = self.depth.checked_sub(reference.depth)?;
        let slot = usize::from(reference.slot);
        if level == 0 {
            return self.names.get(slot).map(String::as_str);
        }
        let mut pattern = self.pattern;
        while let Some(found) = pattern {
            if found.depth == level {
                return found.packs.get(slot).map(String::as_str);
            }
            pattern = found.outer;
        }
        None
    }
}
