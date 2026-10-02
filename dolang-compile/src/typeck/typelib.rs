//! Typelibs: a module's declarations as the checker reads them, without its source.
//!
//! A typelib holds the surface of a module's harvest, with the module's name, path
//! and line table. It is what elaboration starts from, not what it concludes, so a
//! check that loads one elaborates the module's declarations again, as it would from
//! source, but checks none of its bodies.
//!
//! The format is postcard, after a header naming the format and its version. Every
//! ID is local to the typelib, which is linked as any harvest is.

use std::{collections::HashMap, fmt, path::Path};

use serde::{Deserialize, Serialize};

use super::{
    elab::{
        BinderRef, Decl, DeclNode, Harvest, Pending, Referent, Role, Site, Target, UnitInfo,
        surface::{Binder, Decorator, Member, ParamKind, SiteId, StrId},
    },
    r#type::{DeclId, DeclKind, UnitId},
};
use crate::source::Span;

#[cfg(test)]
mod tests;

const MAGIC: [u8; 8] = *b"\xffdotypel";
const VERSION: [u8; 3] = [0, 0, 1];

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Header {
    magic: [u8; 8],
    version: [u8; 3],
}

const HEADER: Header = Header {
    magic: MAGIC,
    version: VERSION,
};

#[derive(Serialize, Deserialize)]
struct Content<'a> {
    module: &'a str,
    /// The module's path, which only locates its diagnostics
    path: &'a str,
    newlines: Vec<u32>,
    #[serde(borrow)]
    strings: Vec<&'a str>,
    decls: Vec<Decl<'a>>,
    sites: Vec<Site>,
    #[serde(borrow)]
    pending: Vec<Pending<'a>>,
    /// By name, so a module's typelib is the same however its exports were hashed
    #[serde(borrow)]
    exports: Vec<Export<'a>>,
}

#[derive(Serialize, Deserialize)]
struct Export<'a> {
    name: &'a str,
    #[serde(with = "wire::span")]
    span: Span,
    #[serde(borrow)]
    target: Target<'a>,
}

/// Why a typelib could not be read
#[derive(Debug)]
pub(crate) enum Invalid {
    /// It does not start with a typelib's header
    Header,
    /// It is of another version of the format
    Version([u8; 3]),
    /// It could not be decoded
    Decode(postcard::Error),
    /// It decoded to something no module's surface could be
    Malformed(&'static str),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Invalid::Header => "not a typelib".fmt(f),
            Invalid::Version([major, minor, patch]) => {
                let [want_major, want_minor, want_patch] = VERSION;
                write!(
                    f,
                    "typelib format {major}.{minor}.{patch} is not supported \
                     (expected {want_major}.{want_minor}.{want_patch})"
                )
            }
            Invalid::Decode(error) => write!(f, "malformed typelib: {error}"),
            Invalid::Malformed(why) => write!(f, "malformed typelib: {why}"),
        }
    }
}

/// Write the typelib of a module's harvest.
pub(crate) fn write(harvest: Harvest<'_>) -> Vec<u8> {
    encode(harvest.surface())
}

/// Encode a harvest that is already a module's surface.
fn encode(harvest: Harvest<'_>) -> Vec<u8> {
    let module = (harvest.info.module).expect("only a module's typelib is written");
    let path = harvest.info.path.to_string_lossy();
    let mut exports: Vec<_> = (harvest.exports.into_iter())
        .map(|(name, (span, target))| Export { name, span, target })
        .collect();
    exports.sort_by_key(|export| export.name);
    let content = Content {
        module,
        path: &path,
        newlines: harvest.info.newlines,
        strings: harvest.strings,
        decls: harvest.decls,
        sites: harvest.sites,
        pending: harvest.pending,
        exports,
    };
    let out = postcard::to_stdvec(&HEADER).expect("a header serializes");
    postcard::to_extend(&content, out).expect("a harvest serializes")
}

/// Read a typelib, checking that it is a module's surface.
pub(crate) fn read(bytes: &[u8]) -> Result<Harvest<'_>, Invalid> {
    let (header, rest): (Header, _) =
        postcard::take_from_bytes(bytes).map_err(|_| Invalid::Header)?;
    if header.magic != MAGIC {
        return Err(Invalid::Header);
    }
    if header.version != VERSION {
        return Err(Invalid::Version(header.version));
    }
    let (content, rest): (Content<'_>, _) =
        postcard::take_from_bytes(rest).map_err(Invalid::Decode)?;
    if !rest.is_empty() {
        return Err(Invalid::Malformed("trailing bytes"));
    }
    let mut exports = HashMap::new();
    for export in content.exports {
        if exports
            .insert(export.name, (export.span, export.target))
            .is_some()
        {
            return Err(Invalid::Malformed("an export is repeated"));
        }
    }
    let mut harvest = Harvest {
        info: UnitInfo {
            module: Some(content.module),
            path: Path::new(content.path),
            newlines: content.newlines,
            source: None,
        },
        strings: content.strings,
        decls: content.decls,
        sites: content.sites,
        pending: content.pending,
        exports,
    };
    validate(&mut harvest).map_err(Invalid::Malformed)?;
    Ok(harvest)
}

/// Check what elaboration assumes of a harvest that collection guarantees.
fn validate(harvest: &mut Harvest<'_>) -> Result<(), &'static str> {
    if !harvest.info.newlines.is_sorted_by(|a, b| a < b) {
        return Err("the line table is out of order");
    }
    let mut ids = InRange {
        decls: harvest.decls.len(),
        sites: harvest.sites.len(),
        strs: harvest.strings.len(),
        ok: true,
    };
    harvest.visit_ids(&mut ids);
    if !ids.ok {
        return Err("an ID is out of range");
    }

    let decls = &harvest.decls;
    for (index, decl) in decls.iter().enumerate() {
        let shaped = match (decl.kind, &decl.node) {
            (DeclKind::Class | DeclKind::Protocol, DeclNode::Class(_)) => true,
            (DeclKind::Alias, DeclNode::Alias(alias)) => alias.body.is_some(),
            (DeclKind::OpaqueAlias, DeclNode::Alias(alias)) => alias.body.is_none(),
            (DeclKind::Function, DeclNode::Defs(defs)) => !defs.is_empty(),
            (DeclKind::Function, DeclNode::Methods(methods)) => !methods.is_empty(),
            // A closure is never on a module's surface
            _ => false,
        };
        if !shaped || decl.name.is_none() {
            return Err("a declaration is malformed");
        }
        match decl.outer {
            // An outer declaration is allocated first
            Some((outer, sig)) if outer.index() >= index || !sig_ok(decls, outer, sig) => {
                return Err("a declaration is nested in one that cannot enclose it");
            }
            _ => {}
        }
        // A method is declared in the class that lists it
        if matches!(decl.node, DeclNode::Methods(_))
            && !decl.outer.is_some_and(|(class, sig)| {
                sig == 0 && matches!(decls[class.index()].node, DeclNode::Class(_))
            })
        {
            return Err("a method is not declared in a class");
        }
        if let DeclNode::Class(class) = &decl.node {
            for member in &class.members {
                if let &Member::Method { decl: method, sig } = member
                    && !(matches!(decls[method.index()].node, DeclNode::Methods(_))
                        && decls[method.index()].outer == Some((DeclId::from_index(index), 0))
                        && sig_ok(decls, method, sig))
                {
                    return Err("a class lists a method it does not declare");
                }
            }
        }
    }

    // Each site is written in one place on the surface, in the role that place gives it
    let sites = &harvest.sites;
    let mut placed = vec![false; sites.len()];
    let mut place = |site: Option<SiteId>, role: Role| match site {
        Some(site) if placed[site.index()] || sites[site.index()].role != role => {
            Err("a site is misplaced")
        }
        Some(site) => {
            placed[site.index()] = true;
            Ok(())
        }
        None => Ok(()),
    };
    for (index, decl) in decls.iter().enumerate() {
        let id = DeclId::from_index(index);
        for sig in 0..sig_count(decl) {
            for (slot, binder) in binders(decl, sig).iter().enumerate() {
                let binder_ref = BinderRef {
                    decl: id,
                    sig,
                    slot,
                };
                place(binder.bound, Role::Bound(binder_ref))?;
                place(binder.default, Role::Default(binder_ref))?;
            }
        }
        match &decl.node {
            DeclNode::Class(class) => {
                for member in &class.members {
                    if let Member::Field(field) = member {
                        place(field.annot, Role::Type)?;
                    }
                }
            }
            DeclNode::Alias(alias) => place(alias.body, Role::Alias(id))?,
            DeclNode::Defs(_) | DeclNode::Methods(_) | DeclNode::Closure(_) => {
                for sig in 0..sig_count(decl) {
                    let signature = decl.node.signature(sig);
                    for param in &signature.params {
                        let role = match param.kind {
                            ParamKind::Rest { pattern: true, .. } => Role::Pattern,
                            ParamKind::Rest { pattern: false, .. } => Role::Rest,
                            ParamKind::Pos | ParamKind::Key { .. } | ParamKind::ConstKey { .. } => {
                                Role::Type
                            }
                        };
                        place(param.annot, role)?;
                    }
                    for site in [signature.input, signature.output, signature.ret] {
                        place(site, Role::Type)?;
                    }
                }
            }
        }
    }
    if placed.contains(&false) {
        return Err("a site is not on the surface");
    }
    for site in sites {
        // A function type takes its omitted ambient channels from a def or method
        let ambient_ok = site.ambient.is_none_or(|(decl, sig)| {
            matches!(
                decls[decl.index()].node,
                DeclNode::Defs(_) | DeclNode::Methods(_)
            ) && sig_ok(decls, decl, sig)
        });
        if !ambient_ok
            || !site
                .owner
                .is_none_or(|(decl, sig)| sig_ok(decls, decl, sig))
        {
            return Err("a site refers to a signature that does not exist");
        }
    }

    // A binder is named only within its declaration
    let mut scopes = HashMap::new();
    for site in sites {
        site.ty.names(&mut |head, _| {
            scopes.insert(head.span, site.group());
        });
    }
    for (index, decl) in decls.iter().enumerate() {
        let group = Some((DeclId::from_index(index), 0));
        match &decl.node {
            DeclNode::Class(class) => {
                for supertype in &class.supers {
                    scopes.insert(supertype.head.span, group);
                    for arg in &supertype.args {
                        arg.ty().names(&mut |head, _| {
                            scopes.insert(head.span, group);
                        });
                    }
                }
            }
            DeclNode::Methods(methods) => {
                for decorator in methods.iter().flat_map(|method| &method.decorators) {
                    if let Decorator::Ident(name) = decorator {
                        scopes.insert(name.span, decl.outer);
                    }
                }
            }
            DeclNode::Alias(_) | DeclNode::Defs(_) | DeclNode::Closure(_) => {}
        }
    }
    for pending in &harvest.pending {
        let Some(&group) = scopes.get(&pending.head.span) else {
            return Err("a name is not on the surface");
        };
        if let Target::Local(Referent::Binder(binder)) = pending.base
            && !(binder_ok(decls, binder) && in_scope(decls, binder, group))
        {
            return Err("a name refers to a binder out of scope");
        }
    }
    for (_, target) in harvest.exports.values() {
        if let Target::Local(Referent::Binder(_)) = target {
            return Err("a binder is exported");
        }
    }
    Ok(())
}

/// Whether `binder` is in scope in the binder group of a declaration signature
fn in_scope(decls: &[Decl<'_>], binder: BinderRef, group: Option<(DeclId, usize)>) -> bool {
    let mut at = group;
    while let Some((decl, sig)) = at {
        if (decl, sig) == (binder.decl, binder.sig) {
            return true;
        }
        at = decls[decl.index()].outer;
    }
    false
}

fn sig_count(decl: &Decl<'_>) -> usize {
    match &decl.node {
        DeclNode::Defs(defs) => defs.len(),
        DeclNode::Methods(methods) => methods.len(),
        DeclNode::Class(_) | DeclNode::Alias(_) | DeclNode::Closure(_) => 1,
    }
}

/// The written binders of signature `sig` of a declaration
fn binders<'d>(decl: &'d Decl<'_>, sig: usize) -> &'d [Binder] {
    match &decl.node {
        DeclNode::Class(class) => &class.binders,
        DeclNode::Alias(alias) => &alias.binders,
        DeclNode::Defs(defs) => &defs[sig].binders,
        DeclNode::Methods(methods) => &methods[sig].binders,
        DeclNode::Closure(_) => &[],
    }
}

/// Whether signature `sig` of `decl` exists
fn sig_ok(decls: &[Decl<'_>], decl: DeclId, sig: usize) -> bool {
    sig < sig_count(&decls[decl.index()])
}

/// Whether a written binder exists
fn binder_ok(decls: &[Decl<'_>], binder: BinderRef) -> bool {
    sig_ok(decls, binder.decl, binder.sig)
        && binder.slot < binders(&decls[binder.decl.index()], binder.sig).len()
}

/// Checks that each ID is in range
struct InRange {
    decls: usize,
    sites: usize,
    strs: usize,
    ok: bool,
}

impl super::elab::Ids for InRange {
    fn unit(&mut self, unit: &mut UnitId) {
        // A typelib has no syntax for any other unit
        self.ok &= *unit == super::elab::local();
    }

    fn decl(&mut self, decl: &mut DeclId) {
        self.ok &= decl.index() < self.decls;
    }

    fn site(&mut self, site: &mut SiteId) {
        self.ok &= site.index() < self.sites;
    }

    fn str(&mut self, id: &mut StrId) {
        self.ok &= id.index() < self.strs;
    }
}

/// Serialization of types the crate exposes, which have no serde impls of their own
pub(crate) mod wire {
    pub(crate) mod span {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        use crate::source::Span;

        pub(crate) fn serialize<S: Serializer>(span: &Span, s: S) -> Result<S::Ok, S::Error> {
            (span.start, span.end).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Span, D::Error> {
            let (start, end) = Deserialize::deserialize(d)?;
            Ok(Span { start, end })
        }
    }

    pub(crate) mod opt_span {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        use crate::source::Span;

        pub(crate) fn serialize<S: Serializer>(
            span: &Option<Span>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            span.map(|span| (span.start, span.end)).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Span>, D::Error> {
            let span: Option<(u32, u32)> = Deserialize::deserialize(d)?;
            Ok(span.map(|(start, end)| Span { start, end }))
        }
    }

    /// A span of a harvest's own unit, which is left implicit
    pub(crate) mod local_span {
        use serde::{Deserializer, Serializer};

        use crate::typeck::{elab::local, r#type::UnitSpan};

        pub(crate) fn serialize<S: Serializer>(span: &UnitSpan, s: S) -> Result<S::Ok, S::Error> {
            debug_assert_eq!(span.unit, local(), "a harvest refers to its own unit");
            super::span::serialize(&span.span, s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<UnitSpan, D::Error> {
            Ok(UnitSpan {
                unit: local(),
                span: super::span::deserialize(d)?,
            })
        }
    }

    pub(crate) mod rest_kind {
        use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

        use crate::RestKind;

        pub(crate) fn serialize<S: Serializer>(kind: &RestKind, s: S) -> Result<S::Ok, S::Error> {
            let tag: u8 = match kind {
                RestKind::Mixed => 0,
                RestKind::Pos => 1,
                RestKind::Key => 2,
            };
            tag.serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RestKind, D::Error> {
            match u8::deserialize(d)? {
                0 => Ok(RestKind::Mixed),
                1 => Ok(RestKind::Pos),
                2 => Ok(RestKind::Key),
                _ => Err(D::Error::custom("unknown rest kind")),
            }
        }
    }
}
