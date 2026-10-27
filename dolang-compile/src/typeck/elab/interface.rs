//! A module's public interface: what its surface keeps, and the private
//! declarations its public declarations name.

use std::fmt::{self, Write};

use rustc_hash::FxHashSet;

use super::{
    Decl, DeclNode, Harvest, Link, Referent, Target,
    surface::{Binder, Member, ModuleId, Name, Ref, Signature, SiteId, StrId, TypeExpr},
};
use crate::{
    diag::{NoteKind, Severity},
    source::Span,
    typeck::{
        report::Report,
        r#type::{DeclId, DeclKind},
    },
};

/// What a harvest's surface keeps
pub(crate) struct Kept {
    /// Whether each declaration is kept
    pub(crate) decls: Vec<bool>,
    /// The fields left out, by class and member index
    pub(crate) fields: FxHashSet<(DeclId, usize)>,
}

/// A private declaration named in the public interface
pub(crate) struct Leak {
    /// The name, where it is written
    pub(crate) span: Span,
    pub(crate) private: DeclId,
    pub(crate) user: User,
}

/// The public part of the interface a [`Leak`] is in
#[derive(Clone, Copy)]
pub(crate) enum User {
    Decl(DeclId),
    /// A field of a class, by name
    Field(DeclId, Name),
    /// An exported variable, by the span of its name
    Value(Span),
}

impl Harvest<'_> {
    /// What the harvest's surface keeps, and the private declarations its interface
    /// names.
    ///
    /// The surface keeps the module's exports and their public and special members,
    /// and any private field whose type is a direct `Phantom` application, since it
    /// counts towards its class's variance. A kept declaration's binders, signatures,
    /// supertypes and kept fields' types are its interface, as is an exported
    /// variable's annotation.
    pub(crate) fn interface(&self) -> (Kept, Vec<Leak>) {
        let exported: FxHashSet<DeclId> = self
            .exports
            .iter()
            .filter_map(|export| match export.target {
                Target::Local(Referent::Decl(decl)) => Some(decl),
                _ => None,
            })
            .collect();

        let mut kept = Kept {
            decls: Vec::with_capacity(self.decls.len()),
            fields: FxHashSet::default(),
        };
        for (index, decl) in self.decls.iter().enumerate() {
            // An outer declaration is allocated first
            let keep = decl.kind != DeclKind::Closure
                && match decl.outer {
                    None => exported.contains(&DeclId::from_index(index)),
                    Some((outer, _)) => {
                        kept.decls[outer.index()]
                            && matches!(self.decls[outer.index()].node, DeclNode::Class(_))
                            && match &decl.node {
                                // Overload signatures share their implementation's
                                // visibility
                                DeclNode::Methods(methods) => {
                                    let primary = methods
                                        .iter()
                                        .find(|method| !method.overload)
                                        .unwrap_or(&methods[0]);
                                    primary.public || primary.special.is_some()
                                }
                                _ => true,
                            }
                    }
                };
            kept.decls.push(keep);
        }

        let mut walk = Walk {
            harvest: self,
            kept: &kept.decls,
            leaks: Vec::new(),
        };
        let mut dropped = FxHashSet::default();
        for (index, decl) in self.decls.iter().enumerate() {
            if !kept.decls[index] {
                continue;
            }
            let id = DeclId::from_index(index);
            let user = User::Decl(id);
            match &decl.node {
                DeclNode::Class(class) => {
                    walk.binders(&class.binders, &user);
                    for supertype in &class.supers {
                        walk.name(&supertype.head, &user);
                        for arg in &supertype.args {
                            walk.ty(arg.ty(), &user);
                        }
                    }
                    for (member, field) in class.members.iter().enumerate() {
                        let Member::Field(field) = field else {
                            continue;
                        };
                        if field.public {
                            // The names share the annotation, which the first stands for
                            if let (Some(annot), Some(&name)) = (field.annot, field.names.first()) {
                                walk.site(annot, &User::Field(id, name));
                            }
                        } else if let Some(annot) = field.annot
                            && walk.phantom(&self.sites[annot.index()].ty)
                        {
                            walk.site(annot, &user);
                        } else {
                            dropped.insert((id, member));
                        }
                    }
                }
                DeclNode::Alias(alias) => {
                    walk.binders(&alias.binders, &user);
                    if let Some(body) = alias.body {
                        walk.site(body, &user);
                    }
                }
                DeclNode::Defs(defs) => {
                    for def in defs {
                        walk.binders(&def.binders, &user);
                        walk.sig(&def.sig, &user);
                    }
                }
                DeclNode::Methods(methods) => {
                    for method in methods {
                        walk.binders(&method.binders, &user);
                        walk.sig(&method.sig, &user);
                    }
                }
                DeclNode::Closure(_) => unreachable!("a closure is left out"),
            }
        }
        for value in &self.values {
            if let Some(annot) = value.annot {
                walk.site(annot, &User::Value(value.span));
            }
        }
        let leaks = walk.leaks;
        kept.fields = dropped;
        (kept, leaks)
    }
}

/// Finds the private declarations an interface names
struct Walk<'h, 'u> {
    harvest: &'h Harvest<'u>,
    kept: &'h [bool],
    leaks: Vec<Leak>,
}

impl Walk<'_, '_> {
    fn binders(&mut self, binders: &[Binder], user: &User) {
        for binder in binders {
            for site in [binder.bound, binder.default].into_iter().flatten() {
                self.site(site, user);
            }
        }
    }

    fn sig(&mut self, sig: &Signature, user: &User) {
        let annots = sig.params.iter().map(|param| param.annot);
        for site in annots.chain([sig.input, sig.output, sig.ret]).flatten() {
            self.site(site, user);
        }
    }

    fn site(&mut self, site: SiteId, user: &User) {
        self.ty(&self.harvest.sites[site.index()].ty, user);
    }

    fn ty(&mut self, ty: &TypeExpr, user: &User) {
        ty.names(&mut |head| self.name(head, user));
    }

    fn link(&self, head: &Ref) -> Option<&Link> {
        head.link.map(|link| &self.harvest.links[link.index()])
    }

    fn str(&self, id: StrId) -> &str {
        &self.harvest.strings[id.index()]
    }

    fn name(&mut self, head: &Ref, user: &User) {
        let Some(link) = self.link(head) else {
            return;
        };
        // A name that isn't a type is diagnosed as such, and the surface erases it
        if let (Target::Local(Referent::Decl(decl)), None) = (link.base, link.item)
            && !self.kept[decl.index()]
            && is_type(self.harvest.decls[decl.index()].kind)
        {
            self.leaks.push(Leak {
                span: head.span,
                private: decl,
                user: *user,
            });
        }
    }

    /// Whether a type is a direct application of std's `Phantom`, as variance
    /// counts it
    fn phantom(&self, ty: &TypeExpr) -> bool {
        let TypeExpr::App { base, .. } = ty.ungrouped() else {
            return false;
        };
        let TypeExpr::Name(head) = base.ungrouped() else {
            return false;
        };
        let Some(link) = self.link(head) else {
            return false;
        };
        let harvest = self.harvest;
        let module = |module: ModuleId| self.str(harvest.modules[module.index()].name);
        match (link.base, link.item) {
            (Target::Import { module: id, item }, None) | (Target::Module(id), Some(item)) => {
                (module(id), self.str(item)) == ("std", "Phantom")
            }
            (Target::Local(Referent::Decl(decl)), None) => {
                let decl = &harvest.decls[decl.index()];
                harvest.info.module == Some("std")
                    && decl.outer.is_none()
                    && decl
                        .name
                        .is_some_and(|name| harvest.strings[name.text.index()] == "Phantom")
            }
            _ => false,
        }
    }
}

fn is_type(kind: DeclKind) -> bool {
    kind.nominal() || kind == DeclKind::Alias
}

fn kind_word(kind: DeclKind) -> &'static str {
    match kind {
        DeclKind::Class => "class",
        DeclKind::Protocol => "protocol",
        DeclKind::OpaqueAlias => "opaque alias",
        DeclKind::Alias => "alias",
        DeclKind::Function | DeclKind::Annotation => "function",
        DeclKind::Closure => "closure",
    }
}

/// A private declaration named in the public interface
pub(crate) struct PrivateInInterface {
    span: Span,
    /// The private declaration's kind and name
    private: (&'static str, String),
    /// The public declaration that names it
    user: String,
}

impl PrivateInInterface {
    pub(crate) fn new(harvest: &Harvest<'_>, leak: &Leak) -> Self {
        let name = |decl: DeclId| {
            let decl = &harvest.decls[decl.index()];
            decl.name
                .map_or("", |name| &harvest.strings[name.text.index()])
        };
        let member = |class: DeclId, member: &str| format!("{}.{member}", name(class));
        let user = match leak.user {
            User::Decl(decl) => match &harvest.decls[decl.index()] {
                Decl {
                    outer: Some((class, _)),
                    node: DeclNode::Methods(methods),
                    ..
                } => {
                    let method = match methods[0].special {
                        Some(_) => format!("({})", name(decl)),
                        None => name(decl).to_owned(),
                    };
                    format!("method `{}`", member(*class, &method))
                }
                _ => {
                    let kind = kind_word(harvest.decls[decl.index()].kind);
                    format!("{kind} `{}`", name(decl))
                }
            },
            User::Field(class, field) => {
                let field = &harvest.strings[field.text.index()];
                format!("field `{}`", member(class, field))
            }
            User::Value(span) => {
                let export = harvest
                    .exports
                    .iter()
                    .find(|export| export.span == span)
                    .expect("an exported variable is exported");
                format!(
                    "exported variable `{}`",
                    harvest.strings[export.name.index()]
                )
            }
        };
        PrivateInInterface {
            span: leak.span,
            private: (
                kind_word(harvest.decls[leak.private.index()].kind),
                name(leak.private).to_owned(),
            ),
            user,
        }
    }
}

impl Report for PrivateInInterface {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        let (kind, name) = &self.private;
        write!(
            w,
            "private {kind} `{name}` in the public interface of {}",
            self.user
        )
    }

    fn span(&self) -> Span {
        self.span
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        let (_, name) = &self.private;
        vec![(NoteKind::Help, format!("declare `{name}` with `pub`"))]
    }
}
