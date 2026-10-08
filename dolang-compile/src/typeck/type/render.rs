//! Rendering types as text, for judgments, diagnostics and traces.
//!
//! The environment a type is rendered in answers questions of fact about it, as
//! [`Names`]; a [`Style`] decides what of it is shown.

use std::{
    borrow::Cow,
    convert::Infallible,
    fmt::{self, Write},
};

use super::{
    Argument, Binder, BinderOrigin, Binding, BoundRef, Database, DeclId, Element, Kind, Literal,
    Multiplicity, Rest, SchemaItem, Type, TypeId, UnionMember,
};

/// What the environment a type is rendered in knows about it
pub(crate) trait Names {
    /// A declaration's qualified name, including a signature declaration's
    fn declaration(&self, id: DeclId) -> Cow<'_, str>;
    /// The collection a declaration's applications are written as, if any
    fn collection(&self, id: DeclId) -> Option<Collection>;
    /// Whether `decl`'s binders are in scope, so its rigids are named bare
    fn in_scope(&self, _decl: DeclId) -> bool {
        false
    }
}

/// A class whose applications have a literal form
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Collection {
    /// `[T]`
    Array,
    /// `(A, B)`
    Tuple,
    /// `(k: A, B)`
    Record,
    /// `{k: A, B}`
    Dict,
}

/// What a rendered type shows
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    /// Everything, as interned
    Full,
    /// As a reader would write it: a rigid by its binder's name, an omitted
    /// channel's by its role, and a function's channels left out where they're
    /// omitted channels' or `Value`, as a function type written outside a
    /// signature takes. Lifted arguments are left out.
    Reader,
    /// As [`Style::Reader`], but with every function's channels shown, to tell
    /// apart types only they do
    Channels,
}

/// A type as the subject of a diagnostic's sentence
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Shown {
    Type(String),
    /// The rigid of a function's omitted channel, which no type names
    Channel {
        /// The function's declaration
        owner: String,
        input: bool,
        bound: Option<String>,
    },
}

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shown::Type(ty) => write!(f, "`{ty}`"),
            Shown::Channel {
                owner,
                input,
                bound,
            } => {
                let role = if *input { "input" } else { "output" };
                write!(f, "the {role} of `{owner}`")?;
                match bound {
                    Some(bound) => write!(f, " (`{bound}`)"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl Database {
    /// A closed type
    pub(crate) fn render(&self, ty: TypeId, names: &dyn Names, style: Style) -> String {
        self.render_in(ty, &[], names, style)
    }

    /// A closed type of the declaration `decl`, its binders named as the
    /// declaration names them, unless its lifted binders are already applied.
    /// Binders it has before the declaration's, as a constructor has its
    /// class's, are named as any quantifier's are.
    pub(crate) fn render_declared(
        &self,
        decl: DeclId,
        ty: TypeId,
        names: &dyn Names,
        style: Style,
    ) -> String {
        let sources = &self.declaration(decl).binders;
        let mut declared: Vec<String> = (sources.iter())
            .map(|binder| self.symbol(binder.name).to_owned())
            .collect();
        match self.ty(ty) {
            Type::Quantified { binders, body } => {
                if binders.len() < declared.len() {
                    let lifted = (sources.iter())
                        .take_while(|binder| binder.origin == BinderOrigin::Lifted)
                        .count();
                    if binders.len() + lifted != declared.len() {
                        return self.render(ty, names, style);
                    }
                    declared.drain(..lifted);
                }
                let renderer = Renderer {
                    db: self,
                    names,
                    style,
                };
                let naming = Naming {
                    names: &[],
                    depth: 0,
                    groups: None,
                };
                let mut out = String::new();
                renderer.quantified(binders, *body, &declared, naming, &mut out);
                out
            }
            _ => self.render(ty, names, style),
        }
    }

    /// A closed type as the subject of a diagnostic's sentence: an omitted
    /// channel's rigid is described, unless `style` shows everything
    pub(crate) fn subject(&self, ty: TypeId, names: &dyn Names, style: Style) -> Shown {
        if style != Style::Full
            && let Type::Rigid { decl, slot, .. } = *self.ty(ty)
            && let Some(input) = self.channel(decl, slot)
        {
            let bound = match self.ty(self.declaration(decl).ty) {
                Type::Quantified { binders, .. } => binders[usize::from(slot)].bound,
                _ => None,
            };
            return Shown::Channel {
                owner: names.declaration(decl).into_owned(),
                input,
                bound: bound.map(|bound| {
                    let bound = self.substitute(bound, &self.rigids(decl));
                    self.render(bound, names, style)
                }),
            };
        }
        Shown::Type(self.render(ty, names, style))
    }

    /// The closed types a diagnostic sets against each other, with every channel
    /// shown if nothing else tells them apart
    pub(crate) fn pair(
        &self,
        found: TypeId,
        expected: TypeId,
        names: &dyn Names,
        style: Style,
    ) -> (Shown, Shown) {
        let pair = (
            self.subject(found, names, style),
            self.subject(expected, names, style),
        );
        match style {
            Style::Reader if pair.0 == pair.1 => self.pair(found, expected, names, Style::Channels),
            _ => pair,
        }
    }

    /// Whether `decl`'s binder `slot` stands for an omitted channel, and if so
    /// whether its input
    fn channel(&self, decl: DeclId, slot: u16) -> Option<bool> {
        let declaration = self.declaration(decl);
        let Type::Quantified { binders, .. } = self.ty(declaration.ty) else {
            return None;
        };
        if binders.get(usize::from(slot))?.binding != Binding::Implicit {
            return None;
        }
        let source = declaration.binders.get(usize::from(slot))?;
        Some(self.symbol(source.name) == "<")
    }

    /// A type with the binders of the group it is interpreted in named by `binders`
    pub(crate) fn render_in(
        &self,
        ty: TypeId,
        binders: &[String],
        names: &dyn Names,
        style: Style,
    ) -> String {
        let renderer = Renderer {
            db: self,
            names,
            style,
        };
        let naming = Naming {
            names: binders,
            depth: 0,
            groups: None,
        };
        let mut out = String::new();
        renderer.render_into(ty, naming, &mut out);
        out
    }

    /// `ty` with each function's channel left out where it's an implicit binder of
    /// the group `depth` groups out, as `implicit` says of its slot
    pub(crate) fn without_channels(
        &self,
        ty: TypeId,
        implicit: &dyn Fn(u16) -> bool,
        depth: u32,
    ) -> TypeId {
        let mapped = self.ty(ty).map_children(|child, groups| {
            Ok::<_, Infallible>(self.without_channels(child, implicit, depth + groups))
        });
        let Ok(mut node) = mapped;
        if let Type::Function(function) = &mut node {
            for channel in [&mut function.input, &mut function.output] {
                if let Some(ty) = *channel
                    && let Type::Bound { reference, .. } = *self.ty(ty)
                    && u32::from(reference.depth) == depth
                    && implicit(reference.slot)
                {
                    *channel = None;
                }
            }
        }
        self.intern(node)
    }
}

struct Renderer<'a> {
    db: &'a Database,
    names: &'a dyn Names,
    style: Style,
}

impl Renderer<'_> {
    fn render_into(&self, ty: TypeId, naming: Naming<'_>, out: &mut String) {
        let db = self.db;
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
            Type::Decl(id) => out.push_str(&self.names.declaration(*id)),
            Type::Rigid { decl, slot, .. } => self.rigid(*decl, *slot, out),
            Type::Bound { reference, .. } => match naming.name(*reference) {
                Some(name) => out.push_str(name),
                None => {
                    let _ = write!(out, "#{}.{}", reference.depth, reference.slot);
                }
            },
            Type::Apply { base, args, .. } => {
                if let Type::Decl(id) = db.ty(*base)
                    && self.collection(*id, args, naming, out)
                {
                    return;
                }
                self.render_into(*base, naming, out);
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
                // The argument for a lone positional schema binder is written as its
                // only item's element when that item repeats
                let mut positional = (binders.iter().enumerate().skip(lifted))
                    .filter(|(_, binder)| !matches!(binder.binding, Binding::Keyword(_)));
                let schema = match (positional.next(), positional.next()) {
                    (Some((index, binder)), None)
                        if binder.kind == Kind::Schema && binder.binding == Binding::Positional =>
                    {
                        Some(index)
                    }
                    _ => None,
                };
                let mut shown_args = Vec::new();
                for (index, arg) in args.iter().enumerate().take(shown) {
                    let mut out = String::new();
                    if index < lifted {
                        match self.style {
                            Style::Full => out.push('^'),
                            Style::Reader | Style::Channels => continue,
                        }
                    }
                    let binder = binders.get(index);
                    match arg {
                        Argument::Positional(ty) => match binder.map(|binder| binder.binding) {
                            Some(Binding::Keyword(name)) => {
                                let _ = write!(out, "{}: ", db.symbol(name));
                                self.render_into(*ty, naming, &mut out)
                            }
                            Some(Binding::Rest(_)) if index >= lifted => {
                                if let Some(items) = self.flat(*ty) {
                                    shown_args.extend(self.arguments(&items, naming));
                                    continue;
                                }
                                self.render_into(*ty, naming, &mut out)
                            }
                            _ if schema == Some(index)
                                && let Some(element) = self.repeated(*ty) =>
                            {
                                match element {
                                    Element::Keyed { key, value } => {
                                        self.render_into(key, naming, &mut out);
                                        out.push_str(", ");
                                        self.render_into(value, naming, &mut out);
                                    }
                                    Element::Positional(ty) | Element::Include(ty) => {
                                        self.render_into(ty, naming, &mut out)
                                    }
                                }
                            }
                            _ => self.render_into(*ty, naming, &mut out),
                        },
                        Argument::Keyword(name, ty) => {
                            let _ = write!(out, "{}: ", db.symbol(*name));
                            self.render_into(*ty, naming, &mut out);
                        }
                        Argument::Expand(ty) => {
                            out.push_str("...");
                            self.render_into(*ty, naming, &mut out);
                        }
                    }
                    shown_args.push(out);
                }
                // Nothing is shown of only lifted arguments
                if lifted > 0 && shown == lifted && self.style != Style::Full {
                    return;
                }
                out.push('[');
                out.push_str(&shown_args.join(", "));
                out.push(']');
            }
            // Its signatures, any of which it's called as
            Type::Overloaded { overloads, .. } => {
                for (index, &overload) in overloads.iter().enumerate() {
                    if index != 0 {
                        out.push_str(" & ");
                    }
                    out.push('(');
                    self.render_into(overload, naming, out);
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
                                self.render_into(*ty, naming, &mut out);
                                out.push(')');
                            }
                            UnionMember::Type(ty) => self.render_into(*ty, naming, &mut out),
                            UnionMember::Expand(ty) => {
                                out.push_str("...");
                                self.render_into(*ty, naming, &mut out);
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
                                self.render_into(*ty, naming, &mut out);
                                out.push(']');
                            }
                            UnionMember::IndexItem(schema, key)
                            | UnionMember::AssignItem(schema, key) => {
                                let name = match member {
                                    UnionMember::IndexItem(..) => "IndexItem",
                                    _ => "AssignItem",
                                };
                                let _ = write!(out, "{name}[");
                                self.render_into(*schema, naming, &mut out);
                                out.push_str(", ");
                                self.render_into(*key, naming, &mut out);
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
                self.items(func.params, naming, out);
                for (sigil, channel) in [('<', func.input), ('>', func.output)] {
                    if let Some(channel) = channel
                        && !self.omits(channel)
                    {
                        if !out.ends_with('(') {
                            out.push_str(", ");
                        }
                        out.push(sigil);
                        self.render_into(channel, naming, out);
                    }
                }
                out.push(')');
                out.push_str(" -> ");
                self.render_into(func.result, naming, out);
            }
            Type::Schema(_) => {
                out.push('{');
                self.items(ty, naming, out);
                out.push('}');
            }
            Type::Quantified { binders, body } => self.quantified(binders, *body, &[], naming, out),
            Type::Map { packs, pattern } => {
                out.push_str("{...");
                self.pattern(packs, *pattern, naming, out);
                out.push('}');
            }
        }
    }

    /// A quantified type, its last binders named by `declared` and the others
    /// made up. Its implicit channels are left out, as they were written, and so
    /// are its implicit binders.
    fn quantified(
        &self,
        binders: &[Binder],
        body: TypeId,
        declared: &[String],
        naming: Naming<'_>,
        out: &mut String,
    ) {
        let db = self.db;
        let implicit = |slot: u16| binders[usize::from(slot)].binding == Binding::Implicit;
        let body = db.without_channels(body, &implicit, 0);
        if binders.is_empty() {
            return self.render_into(body, naming, out);
        }
        let offset = binders.len() - declared.len();
        let mut next = 1;
        let mut listed = Vec::new();
        let names = (binders.iter().enumerate())
            .map(|(slot, binder)| {
                let name = match binder.binding {
                    Binding::Implicit => return None,
                    _ if slot >= offset => declared[slot - offset].clone(),
                    Binding::Keyword(name) => db.symbol(name).to_owned(),
                    _ => loop {
                        let name = format!("T{next}");
                        next += 1;
                        if !naming.taken(&name) && !declared.contains(&name) {
                            break name;
                        }
                    },
                };
                let sigil = match binder.binding {
                    Binding::Keyword(_) => ":",
                    Binding::Rest(Rest::All) => "...",
                    Binding::Rest(Rest::Positional) => "*",
                    Binding::Rest(Rest::Keyed) => "**",
                    _ => "",
                };
                listed.push(format!("{sigil}{name}"));
                Some(name)
            })
            .collect();
        if !listed.is_empty() {
            let _ = write!(out, "@[{}] ", listed.join(", "));
        }
        let group = Group {
            depth: naming.depth + 1,
            names,
            outer: naming.groups,
        };
        self.render_into(body, naming.within(&group), out);
    }

    /// A rigid: everything shows its slot; a reader sees its binder's name, or
    /// for an omitted channel's, its role
    fn rigid(&self, decl: DeclId, slot: u16, out: &mut String) {
        let db = self.db;
        let owner = self.names.declaration(decl);
        if self.style == Style::Full {
            let _ = write!(out, "{owner}.#{slot}");
        } else if let Some(input) = db.channel(decl, slot) {
            let role = if input { "input" } else { "output" };
            let _ = write!(out, "({role} of {owner})");
        } else if let Some(binder) = db.declaration(decl).binders.get(usize::from(slot)) {
            let name = db.symbol(binder.name);
            match self.names.in_scope(decl) {
                true => out.push_str(name),
                false => {
                    let _ = write!(out, "{owner}.{name}");
                }
            }
        } else {
            let _ = write!(out, "{owner}.#{slot}");
        }
    }

    /// Whether a function's channel is left out (see [`Style::Reader`])
    fn omits(&self, channel: TypeId) -> bool {
        let db = self.db;
        self.style == Style::Reader
            && match *db.ty(channel) {
                Type::Rigid { decl, slot, .. } => db.channel(decl, slot).is_some(),
                _ => channel == db.top(),
            }
    }

    /// An application of a collection class in its literal form, if it has one
    fn collection(
        &self,
        id: DeclId,
        args: &[Argument],
        naming: Naming<'_>,
        out: &mut String,
    ) -> bool {
        let Some(collection) = self.names.collection(id) else {
            return false;
        };
        let [Argument::Positional(arg)] = *args else {
            return false;
        };
        if collection == Collection::Array {
            out.push('[');
            self.render_into(arg, naming, out);
            out.push(']');
            return true;
        }
        let Some(items) = self.flat(arg) else {
            return false;
        };
        let keyed = (items.iter()).any(|item| matches!(item.element, Element::Keyed { .. }));
        let (open, close) = match collection {
            Collection::Array => unreachable!(),
            Collection::Tuple => match *items {
                _ if keyed => return false,
                // A lone item would be grouping
                [ref item] if item.multiplicity == Multiplicity::Required => match item.element {
                    Element::Positional(_) => ("(", ",)"),
                    _ => return false,
                },
                _ => ("(", ")"),
            },
            // Only an explicitly keyed item makes parentheses a record
            Collection::Record if keyed => ("(", ")"),
            Collection::Record => return false,
            // A lone repeated item is written as arguments
            Collection::Dict if self.repeated(arg).is_some() => return false,
            Collection::Dict => ("{", "}"),
        };
        out.push_str(open);
        self.item_list(&items, naming, out);
        out.push_str(close);
        true
    }

    /// The element of a literal schema's only item, if it repeats and isn't an
    /// inclusion
    fn repeated(&self, ty: TypeId) -> Option<Element> {
        match self.flat(ty)?.as_slice() {
            [item]
                if item.multiplicity == Multiplicity::Repeated
                    && !matches!(item.element, Element::Include(_)) =>
            {
                Some(item.element.clone())
            }
            _ => None,
        }
    }

    /// A literal schema's items, with the items of the literal schemas it
    /// includes in place of their inclusions
    fn flat(&self, ty: TypeId) -> Option<Vec<SchemaItem>> {
        let Type::Schema(items) = self.db.ty(ty) else {
            return None;
        };
        let mut flat = Vec::with_capacity(items.len());
        for item in items.iter() {
            if item.multiplicity == Multiplicity::Required
                && let Element::Include(included) = item.element
                && let Some(included) = self.flat(included)
            {
                flat.extend(included);
            } else {
                flat.push(item.clone());
            }
        }
        Some(flat)
    }

    /// A pack's items as the type arguments that give them
    fn arguments(&self, items: &[SchemaItem], naming: Naming<'_>) -> Vec<String> {
        let db = self.db;
        let mut args = Vec::new();
        // Items with no argument of their own are included together
        let mut rest = Vec::new();
        let flush = |rest: &mut Vec<SchemaItem>, args: &mut Vec<String>| {
            if !rest.is_empty() {
                let mut out = String::from("...{");
                self.item_list(rest, naming, &mut out);
                out.push('}');
                args.push(out);
                rest.clear();
            }
        };
        for item in items {
            let mut out = String::new();
            match (item.multiplicity, &item.element) {
                (Multiplicity::Required, Element::Positional(ty)) => {
                    self.render_into(*ty, naming, &mut out)
                }
                (Multiplicity::Required, Element::Keyed { key, .. })
                    if matches!(db.ty(*key), Type::Literal(Literal::Sym(_))) =>
                {
                    self.item(item, naming, &mut out)
                }
                (Multiplicity::Repeated, Element::Positional(_))
                | (Multiplicity::Required, Element::Include(_)) => {
                    self.item(item, naming, &mut out);
                    // `*T` is given as `...T`
                    if out.starts_with('*') {
                        out.replace_range(..1, "...");
                    }
                }
                _ => {
                    rest.push(item.clone());
                    continue;
                }
            }
            flush(&mut rest, &mut args);
            args.push(out);
        }
        flush(&mut rest, &mut args);
        args
    }

    /// A mapping's pattern, as written: each item is named by its pack
    fn pattern(&self, packs: &[TypeId], pattern: TypeId, naming: Naming<'_>, out: &mut String) {
        let group = Group {
            depth: naming.depth + 1,
            names: (packs.iter())
                .map(|&pack| {
                    let mut out = String::new();
                    self.render_into(pack, naming, &mut out);
                    Some(out)
                })
                .collect(),
            outer: naming.groups,
        };
        self.render_into(pattern, naming.within(&group), out);
    }

    /// The items of a schema, or what stands for one
    fn items(&self, ty: TypeId, naming: Naming<'_>, out: &mut String) {
        match self.flat(ty) {
            Some(items) => self.item_list(&items, naming, out),
            None => {
                out.push_str("...");
                self.render_into(ty, naming, out);
            }
        }
    }

    fn item_list(&self, items: &[SchemaItem], naming: Naming<'_>, out: &mut String) {
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                out.push_str(", ");
            }
            self.item(item, naming, out);
        }
    }

    fn item(&self, item: &SchemaItem, naming: Naming<'_>, out: &mut String) {
        let db = self.db;
        out.push_str(match item.multiplicity {
            Multiplicity::Required => "",
            Multiplicity::Optional => "?",
            Multiplicity::Repeated => "*",
        });
        match item.element {
            Element::Positional(ty) => self.render_into(ty, naming, out),
            Element::Keyed { key, value } => {
                match db.ty(key) {
                    Type::Literal(Literal::Sym(sym)) => out.push_str(db.symbol(*sym)),
                    _ => {
                        out.push('(');
                        self.render_into(key, naming, out);
                        out.push(')');
                    }
                }
                out.push_str(": ");
                self.render_into(value, naming, out);
            }
            Element::Include(ty) => {
                out.push_str("...");
                match db.ty(ty) {
                    Type::Map { packs, pattern } => self.pattern(packs, *pattern, naming, out),
                    _ => self.render_into(ty, naming, out),
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
    /// The innermost group entered whose binders are named
    groups: Option<&'a Group<'a>>,
}

/// A group entered while rendering: a quantifier's, or a mapping's pattern's
struct Group<'a> {
    /// The depth of the group
    depth: u16,
    /// Each binder's name, if it has one
    names: Vec<Option<String>>,
    outer: Option<&'a Group<'a>>,
}

impl<'a> Naming<'a> {
    fn enter(self) -> Self {
        Self {
            depth: self.depth + 1,
            ..self
        }
    }

    /// Entering `group`
    fn within<'b>(self, group: &'b Group<'b>) -> Naming<'b>
    where
        'a: 'b,
    {
        Naming {
            groups: Some(group),
            ..self.enter()
        }
    }

    /// The name of a reference to the rendered type's group or an entered one
    fn name(&self, reference: BoundRef) -> Option<&str> {
        let level = self.depth.checked_sub(reference.depth)?;
        let slot = usize::from(reference.slot);
        if level == 0 {
            return self.names.get(slot).map(String::as_str);
        }
        let mut group = self.groups;
        while let Some(found) = group {
            if found.depth == level {
                return found.names.get(slot)?.as_deref();
            }
            group = found.outer;
        }
        None
    }

    /// Whether `name` already names a binder
    fn taken(&self, name: &str) -> bool {
        let mut group = self.groups;
        while let Some(found) = group {
            if found.names.iter().flatten().any(|taken| taken == name) {
                return true;
            }
            group = found.outer;
        }
        self.names.iter().any(|taken| taken == name)
    }
}
