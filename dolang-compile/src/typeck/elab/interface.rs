//! A module's public interface: what its surface keeps, and the private
//! declarations its public declarations name.

use std::fmt::{self, Write};

use rustc_hash::FxHashSet;

use super::{
    Bases, Decl, DeclNode, Link, Referent, Shard, Target,
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

/// What a shard's surface keeps
pub(crate) struct Kept {
    /// Whether each declaration is kept, by its offset in the unit
    pub(crate) decls: Vec<bool>,
    /// The fields left out, by their class's offset in the unit and member index
    pub(crate) fields: FxHashSet<(usize, usize)>,
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

impl Shard<'_> {
    /// What the shard's surface keeps, and the private declarations its interface
    /// names. The shard's IDs start at `base`.
    ///
    /// The surface keeps the module's exports and their public and special members,
    /// and any private field whose type is a direct `Phantom` application, since it
    /// counts towards its class's variance. A kept declaration's binders, signatures,
    /// supertypes and kept fields' types are its interface, as is an exported
    /// variable's annotation.
    pub(crate) fn interface(&self, base: &Bases) -> (Kept, Vec<Leak>) {
        let exported: FxHashSet<usize> = self
            .exports
            .iter()
            .filter_map(|export| match export.target {
                Target::Local(Referent::Decl(decl)) => Some(decl.index() - base.decls),
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
                    None => exported.contains(&index),
                    Some((outer, _)) => {
                        let outer = outer.index() - base.decls;
                        kept.decls[outer]
                            && matches!(self.decls[outer].node, DeclNode::Class(_))
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
            shard: self,
            base,
            kept: &kept.decls,
            leaks: Vec::new(),
        };
        let mut dropped = FxHashSet::default();
        for (index, decl) in self.decls.iter().enumerate() {
            if !kept.decls[index] {
                continue;
            }
            let id = DeclId::from_index(base.decls + index);
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
                            && walk.phantom(&self.sites[annot.index() - base.sites].ty)
                        {
                            walk.site(annot, &user);
                        } else {
                            dropped.insert((index, member));
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
    shard: &'h Shard<'u>,
    base: &'h Bases,
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
        self.ty(&self.shard.sites[site.index() - self.base.sites].ty, user);
    }

    fn ty(&mut self, ty: &TypeExpr, user: &User) {
        ty.names(&mut |head| self.name(head, user));
    }

    fn link(&self, head: &Ref) -> Option<&Link> {
        head.link
            .map(|link| &self.shard.links[link.index() - self.base.links])
    }

    fn str(&self, id: StrId) -> &str {
        &self.shard.strings[id.index()]
    }

    fn name(&mut self, head: &Ref, user: &User) {
        let Some(link) = self.link(head) else {
            return;
        };
        // A name that isn't a type is diagnosed as such, and the surface erases it
        if let (Target::Local(Referent::Decl(decl)), None) = (link.base, link.item)
            && !self.kept[decl.index() - self.base.decls]
            && is_type(self.shard.decls[decl.index() - self.base.decls].kind)
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
        let shard = self.shard;
        let module = |module: ModuleId| self.str(shard.modules[module.index()].name);
        match (link.base, link.item) {
            (Target::Import { module: id, item }, None) | (Target::Module(id), Some(item)) => {
                (module(id), self.str(item)) == ("std", "Phantom")
            }
            (Target::Local(Referent::Decl(decl)), None) => {
                let decl = &shard.decls[decl.index() - self.base.decls];
                shard.info.module == Some("std")
                    && decl.outer.is_none()
                    && decl
                        .name
                        .is_some_and(|name| shard.strings[name.text.index()] == "Phantom")
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
    /// The leak's report, for a shard whose IDs start at `base`
    pub(crate) fn new(shard: &Shard<'_>, base: &Bases, leak: &Leak) -> Self {
        let decl = |id: DeclId| &shard.decls[id.index() - base.decls];
        let name = |id: DeclId| {
            decl(id)
                .name
                .map_or("", |name| &shard.strings[name.text.index()])
        };
        let member = |class: DeclId, member: &str| format!("{}.{member}", name(class));
        let user = match leak.user {
            User::Decl(id) => match decl(id) {
                Decl {
                    outer: Some((class, _)),
                    node: DeclNode::Methods(methods),
                    ..
                } => {
                    let method = match methods[0].special {
                        Some(_) => format!("({})", name(id)),
                        None => name(id).to_owned(),
                    };
                    format!("method `{}`", member(*class, &method))
                }
                _ => {
                    let kind = kind_word(decl(id).kind);
                    format!("{kind} `{}`", name(id))
                }
            },
            User::Field(class, field) => {
                let field = &shard.strings[field.text.index()];
                format!("field `{}`", member(class, field))
            }
            User::Value(span) => {
                let export = shard
                    .exports
                    .iter()
                    .find(|export| export.span == span)
                    .expect("an exported variable is exported");
                format!("exported variable `{}`", shard.strings[export.name.index()])
            }
        };
        PrivateInInterface {
            span: leak.span,
            private: (
                kind_word(decl(leak.private).kind),
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
