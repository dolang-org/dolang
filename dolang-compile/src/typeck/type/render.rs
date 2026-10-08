//! Rendering types as text, for judgments, diagnostics and traces.
//!
//! The environment a type is rendered in answers questions of fact about it, as
//! [`Names`]; a [`Style`] decides what of it is shown.

use std::{borrow::Cow, convert::Infallible, fmt::Write};

use super::{
    Argument, Binder, BinderOrigin, Binding, BoundRef, Database, DeclId, Element, Kind, Literal,
    Multiplicity, Type, TypeId, UnionMember,
};

/// What the environment a type is rendered in knows about it
pub(crate) trait Names {
    /// A declaration's qualified name, including a signature declaration's
    fn declaration(&self, id: DeclId) -> Cow<'_, str>;
}

/// What a rendered type shows
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    /// Everything, as interned
    Full,
}

impl Database {
    /// A closed type
    pub(crate) fn render(&self, ty: TypeId, names: &dyn Names, style: Style) -> String {
        self.render_in(ty, &[], names, style)
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
            pattern: None,
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
    #[expect(dead_code, reason = "every style shows everything for now")]
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
            Type::Rigid { decl, slot, .. } => {
                let _ = write!(out, "{}.#{slot}", self.names.declaration(*decl));
            }
            Type::Bound { reference, .. } => match naming.name(*reference) {
                Some(name) => out.push_str(name),
                None => {
                    let _ = write!(out, "#{}.{}", reference.depth, reference.slot);
                }
            },
            Type::Apply { base, args, .. } => {
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
                            self.render_into(*ty, naming, out)
                        }
                        Argument::Keyword(name, ty) => {
                            let _ = write!(out, "{}: ", db.symbol(*name));
                            self.render_into(*ty, naming, out);
                        }
                        Argument::Expand(ty) => {
                            out.push_str("...");
                            self.render_into(*ty, naming, out);
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
                    if let Some(channel) = channel {
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
            // Its implicit channels are left out, as they were written
            Type::Quantified { binders, body } => {
                let implicit = |slot: u16| binders[usize::from(slot)].binding == Binding::Implicit;
                let body = db.without_channels(*body, &implicit, 0);
                out.push_str("forall ");
                self.render_into(body, naming.enter(), out);
            }
            Type::Map { packs, pattern } => {
                out.push_str("{...");
                self.pattern(packs, *pattern, naming, out);
                out.push('}');
            }
        }
    }

    /// A mapping's pattern, as written: each item is named by its pack
    fn pattern(&self, packs: &[TypeId], pattern: TypeId, naming: Naming<'_>, out: &mut String) {
        let items = Pattern {
            depth: naming.depth + 1,
            packs: (packs.iter())
                .map(|&pack| {
                    let mut out = String::new();
                    self.render_into(pack, naming, &mut out);
                    out
                })
                .collect(),
            outer: naming.pattern,
        };
        let naming = Naming {
            pattern: Some(&items),
            ..naming.enter()
        };
        self.render_into(pattern, naming, out);
    }

    /// The items of a schema, or what stands for one
    fn items(&self, ty: TypeId, naming: Naming<'_>, out: &mut String) {
        let db = self.db;
        let Type::Schema(items) = db.ty(ty) else {
            out.push_str("...");
            return self.render_into(ty, naming, out);
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
