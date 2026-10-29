//! A typelib's serialization form, and its conversion to and from a harvest.
//!
//! The form follows the harvest, except that it is denser:
//!
//! - Type expressions are interned in one table, each distinct node once, with its
//!   children before it. A site, or a supertype's argument, refers to a node.
//! - What can be derived from the rest is left out where it holds: a site's owner
//!   and ambient signature, a def's or method's name where it is its declaration's,
//!   a parameter's key where it is the parameter's name, and a class's methods
//!   among its members. Where a derivation doesn't hold, the value is written.
//! - A declaration's kind is its node's, flags are packed into a byte, and an
//!   optional ID is written as one more than the ID, or 0.

use std::{borrow::Cow, path::Path};

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::{
    RestKind,
    ast::SpecialMethod,
    source::Span,
    typeck::{
        elab::{
            self, BinderRef, DeclNode, Harvest, Referent, Role, UnitInfo, local,
            surface::{
                self, ConstLit, Decorator, LinkId, Member, MemberScope, ModuleId, ParamKind, Ref,
                SiteId, StrId, Symbol, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeQuant,
            },
        },
        r#type::{DeclId, DeclKind, UnitSpan},
    },
};

/// A typelib's content, its strings either borrowed from the bytes it was read
/// from or owned by the check that keeps it
#[derive(Serialize, Deserialize)]
pub(super) struct Content<S> {
    pub(super) module: u32,
    /// The module's path, which only locates its diagnostics
    pub(super) path: u32,
    /// The number and start of each line holding a span the typelib keeps
    pub(super) lines: Vec<(u32, u32)>,
    /// Whether the module was checked strictly, which its signatures depend on
    pub(super) strict: bool,
    pub(super) strings: Vec<S>,
    /// Each distinct type expression node, after its children
    pub(super) types: Vec<Type>,
    decls: Vec<Decl>,
    sites: Vec<Site>,
    links: Vec<Link>,
    exports: Vec<Export>,
    values: Vec<Value>,
    modules: Vec<Module>,
}

impl<'a> Content<&'a str> {
    /// The names of the modules the module imports, each once, in order
    pub(super) fn imports(&self) -> Vec<&'a str> {
        let mut imports: Vec<&str> = self
            .modules
            .iter()
            .filter_map(|module| self.strings.get(module.name as usize).copied())
            .collect();
        imports.sort_unstable();
        imports
    }
}

/// A span's start and end
type WireSpan = (u32, u32);

/// A declaration signature
type Sig = (u32, u32);

/// An optional ID, as one more than the ID, or 0
type Opt = u32;

#[derive(Serialize, Deserialize)]
struct Name {
    span: WireSpan,
    text: u32,
}

#[derive(Serialize, Deserialize)]
struct Decl {
    name: Name,
    node: Node,
    outer: Option<Sig>,
}

/// A declaration's node, which gives its kind. A closure is never on a surface.
#[derive(Serialize, Deserialize)]
enum Node {
    Class(Class),
    Protocol(Class),
    /// A transparent alias, or an opaque one where it has no body
    Alias {
        binders: Vec<Binder>,
        body: Opt,
    },
    Defs(Vec<Def>),
    Methods(Vec<Method>),
}

#[derive(Serialize, Deserialize)]
struct Class {
    binders: Vec<Binder>,
    supers: Vec<Super>,
    members: Members,
    decorators: Vec<WireDecorator>,
}

#[derive(Serialize, Deserialize)]
enum Members {
    /// The fields, among which the methods declared in the class are placed by the
    /// spans of their names
    Derived(Vec<Field>),
    Written(Vec<WireMember>),
}

#[derive(Serialize, Deserialize)]
enum WireMember {
    Field(Field),
    Method(Sig),
}

#[derive(Serialize, Deserialize)]
struct Field {
    names: Vec<Name>,
    annot: Opt,
    /// [`PUBLIC`] and the scope
    flags: u8,
}

#[derive(Serialize, Deserialize)]
struct Super {
    head: Opt,
    args: Vec<Arg>,
    type_only: bool,
}

#[derive(Serialize, Deserialize)]
enum WireDecorator {
    /// A bare name, by its link
    Ident(Opt),
    Other,
}

#[derive(Serialize, Deserialize)]
struct Binder {
    kind: BinderKind,
    name: Name,
    bound: Opt,
    default: Opt,
}

#[derive(Serialize, Deserialize)]
enum BinderKind {
    Pos,
    Key,
    Rest(Rest),
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum Rest {
    Mixed,
    Pos,
    Key,
}

#[derive(Serialize, Deserialize)]
struct Def {
    /// Absent where it is the declaration's
    name: Option<Name>,
    binders: Vec<Binder>,
    sig: Signature,
    type_only: bool,
}

#[derive(Serialize, Deserialize)]
struct Method {
    /// Absent where it is the declaration's
    name: Option<Name>,
    /// [`OVERLOAD`], [`PUBLIC`] and the scope
    flags: u8,
    special: Option<SpecialMethod>,
    binders: Vec<Binder>,
    sig: Signature,
    decorators: Vec<WireDecorator>,
}

#[derive(Serialize, Deserialize)]
struct Signature {
    params: Vec<Param>,
    input: Opt,
    output: Opt,
    ret: Opt,
}

#[derive(Serialize, Deserialize)]
struct Param {
    kind: WireParamKind,
    name: Option<Name>,
    default: bool,
    annot: Opt,
}

#[derive(Serialize, Deserialize)]
enum WireParamKind {
    Pos,
    /// Passed by a symbol key, absent where it is the parameter's name
    Key(Option<Name>),
    ConstKey(Option<Const>),
    Rest {
        kind: Rest,
        pattern: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct Site {
    ty: u32,
    role: WireRole,
    /// Absent where it is the owner the site's place on the surface gives it
    owner: Option<Option<Sig>>,
    ambient: Ambient,
}

#[derive(Serialize, Deserialize)]
enum WireRole {
    Type,
    Rest,
    Pattern,
    Bound(WireBinder),
    Default(WireBinder),
    Alias(u32),
}

/// A binder: its declaration, signature and slot
type WireBinder = (u32, u32, u32);

#[derive(Serialize, Deserialize)]
enum Ambient {
    None,
    /// The signature whose binder group the site is interpreted in
    Group,
    Sig(Sig),
}

#[derive(Serialize, Deserialize)]
struct Link {
    base: WireTarget,
    item: Opt,
}

#[derive(Serialize, Deserialize)]
enum WireTarget {
    Decl(u32),
    Binder(WireBinder),
    Value(WireSpan),
    Error,
    Import { module: u32, item: u32 },
    Module(u32),
}

#[derive(Serialize, Deserialize)]
struct Export {
    name: u32,
    span: WireSpan,
    target: WireTarget,
}

#[derive(Serialize, Deserialize)]
struct Value {
    span: WireSpan,
    annot: Opt,
}

#[derive(Serialize, Deserialize)]
struct Module {
    name: u32,
    span: WireSpan,
}

/// A type expression node, whose children are nodes of the table before it
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Type {
    /// A name, by its link
    Name(Opt),
    Const(Option<Const>),
    App {
        base: u32,
        args: Vec<Arg>,
    },
    Schema {
        params: Vec<WireParam>,
        tuple: bool,
    },
    Group(u32),
    Tuple(Vec<WireParam>),
    Record(Vec<WireParam>),
    Array(u32),
    Union(Vec<u32>),
    Func {
        params: Vec<WireParam>,
        input: Opt,
        output: Opt,
        ret: u32,
    },
    Error,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Const {
    Str(u32),
    Int(i128),
    Bool(bool),
    Nil,
    Sym(u32),
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Arg {
    Pos(u32),
    Key { name: u32, ty: u32 },
    Expand(u32),
}

/// An item of a schema or a function type's parameters
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum WireParam {
    Pos {
        quant: Option<Quant>,
        ty: u32,
    },
    Key {
        quant: Option<Quant>,
        key: Key,
        ty: u32,
    },
    AnyPos,
    AnyKey(Opt),
    Open,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Quant {
    Opt,
    Star,
    Include,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Key {
    Sym(u32),
    Type(u32),
}

const OVERLOAD: u8 = 1;
const PUBLIC: u8 = 2;
/// Where a member's scope starts in its flags
const SCOPE_SHIFT: u32 = 2;

fn scope_flags(scope: MemberScope) -> u8 {
    let scope: u8 = match scope {
        MemberScope::Instance => 0,
        MemberScope::Class => 1,
        MemberScope::Static => 2,
    };
    scope << SCOPE_SHIFT
}

/// A member's scope, from flags of which only `known` and the scope may be set
fn scope(flags: u8, known: u8) -> Result<MemberScope, &'static str> {
    if flags & ((1 << SCOPE_SHIFT) - 1) & !known != 0 {
        return Err("unknown flags");
    }
    match flags >> SCOPE_SHIFT {
        0 => Ok(MemberScope::Instance),
        1 => Ok(MemberScope::Class),
        2 => Ok(MemberScope::Static),
        _ => Err("unknown flags"),
    }
}

fn opt(id: Option<usize>) -> Opt {
    id.map_or(0, |id| u32::try_from(id + 1).expect("an ID fits in a u32"))
}

fn index(id: usize) -> u32 {
    u32::try_from(id).expect("an ID fits in a u32")
}

fn span(span: Span) -> WireSpan {
    (span.start, span.end)
}

fn unspan((start, end): WireSpan) -> Span {
    Span { start, end }
}

fn sig((decl, sig): (DeclId, usize)) -> Sig {
    (index(decl.index()), index(sig))
}

fn binder(binder: BinderRef) -> WireBinder {
    (
        index(binder.decl.index()),
        index(binder.sig),
        index(binder.slot),
    )
}

fn rest(kind: RestKind) -> Rest {
    match kind {
        RestKind::Mixed => Rest::Mixed,
        RestKind::Pos => Rest::Pos,
        RestKind::Key => Rest::Key,
    }
}

fn unrest(rest: Rest) -> RestKind {
    match rest {
        Rest::Mixed => RestKind::Mixed,
        Rest::Pos => RestKind::Pos,
        Rest::Key => RestKind::Key,
    }
}

/// The owner each site's place on a surface gives it, or `None` for a site placed
/// nowhere. A function type at a def's or method's signature takes its omitted
/// channels from the signature it is interpreted in; elsewhere it has none.
fn placed(decls: &[elab::Decl<'_>], values: &[elab::Value], sites: usize) -> Vec<Placed> {
    let mut placed = vec![None; sites];
    let mut place = |site: Option<SiteId>, owner: Option<(DeclId, usize)>| {
        if let Some(slot) = site.and_then(|site| placed.get_mut(site.index())) {
            *slot = Some(owner);
        }
    };
    for (index, decl) in decls.iter().enumerate() {
        let id = DeclId::from_index(index);
        let binders: Vec<&[surface::Binder]> = match &decl.node {
            DeclNode::Class(class) => vec![&class.binders],
            DeclNode::Alias(alias) => vec![&alias.binders],
            DeclNode::Defs(defs) => defs.iter().map(|def| &def.binders[..]).collect(),
            DeclNode::Methods(methods) => {
                methods.iter().map(|method| &method.binders[..]).collect()
            }
            DeclNode::Closure(_) => Vec::new(),
        };
        // A binder's bound and default are written where its declaration is
        for binder in binders.into_iter().flatten() {
            place(binder.bound, decl.outer);
            place(binder.default, decl.outer);
        }
        match &decl.node {
            DeclNode::Class(class) => {
                for member in &class.members {
                    if let Member::Field(field) = member {
                        place(field.annot, Some((id, 0)));
                    }
                }
            }
            DeclNode::Alias(alias) => place(alias.body, decl.outer),
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for index in 0..decl.node.sig_count() {
                    let signature = decl.node.signature(index);
                    let sites = signature.params.iter().map(|param| param.annot);
                    let channels = [signature.input, signature.output, signature.ret];
                    for site in sites.chain(channels) {
                        place(site, Some((id, index)));
                    }
                }
            }
            DeclNode::Closure(_) => {}
        }
    }
    for value in values {
        place(value.annot, None);
    }
    placed
}

/// A site's owner, if it is placed on the surface
type Placed = Option<Option<(DeclId, usize)>>;

/// The ambient signature a site's place gives it
fn ambient(decls: &[elab::Decl<'_>], site: &elab::Site) -> Option<(DeclId, usize)> {
    site.group().filter(|(decl, _)| {
        decls
            .get(decl.index())
            .is_some_and(|decl| matches!(decl.node, DeclNode::Defs(_) | DeclNode::Methods(_)))
    })
}

/// The members of a class with fields `fields`: they and the method signatures
/// declared in it, in the order of the spans of their names
fn members(decls: &[elab::Decl<'_>], class: DeclId, fields: Vec<surface::Field>) -> Vec<Member> {
    let mut members: Vec<(u32, Member)> = fields
        .into_iter()
        .map(|field| {
            let start = field.names.first().map_or(0, |name| name.span.start);
            (start, Member::Field(field))
        })
        .collect();
    for (index, decl) in decls.iter().enumerate() {
        if decl.outer != Some((class, 0)) {
            continue;
        }
        let DeclNode::Methods(methods) = &decl.node else {
            continue;
        };
        let decl = DeclId::from_index(index);
        for (sig, method) in methods.iter().enumerate() {
            members.push((method.name.span.start, Member::Method { decl, sig }));
        }
    }
    members.sort_by_key(|(start, _)| *start);
    members.into_iter().map(|(_, member)| member).collect()
}

/// Converts a harvest that is a module's surface to its serialization form
pub(super) struct Encode<'h, 'u> {
    harvest: &'h Harvest<'u>,
    types: Vec<Type>,
    interned: FxHashMap<Type, u32>,
}

impl<'h, 'u> Encode<'h, 'u> {
    pub(super) fn new(harvest: &'h Harvest<'u>) -> Self {
        Self {
            harvest,
            types: Vec::new(),
            interned: FxHashMap::default(),
        }
    }

    /// The content, with the given header fields
    pub(super) fn content(
        mut self,
        module: StrId,
        path: StrId,
        lines: Vec<(u32, u32)>,
        strings: Vec<String>,
    ) -> Content<String> {
        let harvest = self.harvest;
        let decls = harvest
            .decls
            .iter()
            .enumerate()
            .map(|(index, decl)| self.decl(DeclId::from_index(index), decl))
            .collect();
        let placed = placed(&harvest.decls, &harvest.values, harvest.sites.len());
        let sites = harvest
            .sites
            .iter()
            .zip(placed)
            .map(|(site, placed)| self.site(site, placed))
            .collect();
        Content {
            module: index(module.index()),
            path: index(path.index()),
            lines,
            strict: harvest.info.strict,
            strings,
            types: self.types,
            decls,
            sites,
            links: harvest
                .links
                .iter()
                .map(|link| Link {
                    base: target(link.base),
                    item: opt(link.item.map(StrId::index)),
                })
                .collect(),
            exports: harvest
                .exports
                .iter()
                .map(|export| Export {
                    name: index(export.name.index()),
                    span: span(export.span),
                    target: target(export.target),
                })
                .collect(),
            values: harvest
                .values
                .iter()
                .map(|value| Value {
                    span: span(value.span),
                    annot: opt(value.annot.map(SiteId::index)),
                })
                .collect(),
            modules: harvest
                .modules
                .iter()
                .map(|module| Module {
                    name: index(module.name.index()),
                    span: span(module.span),
                })
                .collect(),
        }
    }

    fn decl(&mut self, id: DeclId, decl: &elab::Decl<'_>) -> Decl {
        let own = decl.name.expect("a declaration on a surface is named");
        // A def's or method's name is mostly its declaration's
        let named = |name: surface::Name| (name != own).then(|| self::name(name));
        let node = match (&decl.node, decl.kind) {
            (DeclNode::Class(class), kind) => {
                let class = self.class(id, class);
                match kind {
                    DeclKind::Protocol => Node::Protocol(class),
                    _ => Node::Class(class),
                }
            }
            (DeclNode::Alias(alias), _) => Node::Alias {
                binders: self.binders(&alias.binders),
                body: opt(alias.body.map(SiteId::index)),
            },
            (DeclNode::Defs(defs), _) => Node::Defs(
                defs.iter()
                    .map(|def| Def {
                        name: named(def.name),
                        binders: self.binders(&def.binders),
                        sig: self.signature(&def.sig),
                        type_only: def.type_only,
                    })
                    .collect(),
            ),
            (DeclNode::Methods(methods), _) => Node::Methods(
                methods
                    .iter()
                    .map(|method| {
                        let mut flags = scope_flags(method.scope);
                        if method.overload {
                            flags |= OVERLOAD;
                        }
                        if method.public {
                            flags |= PUBLIC;
                        }
                        Method {
                            name: named(method.name),
                            flags,
                            special: method.special,
                            binders: self.binders(&method.binders),
                            sig: self.signature(&method.sig),
                            decorators: decorators(&method.decorators),
                        }
                    })
                    .collect(),
            ),
            (DeclNode::Closure(_), _) => unreachable!("a closure is never on a surface"),
        };
        Decl {
            name: name(own),
            node,
            outer: decl.outer.map(sig),
        }
    }

    fn class(&mut self, id: DeclId, class: &surface::Class) -> Class {
        let fields: Vec<_> = class
            .members
            .iter()
            .filter_map(|member| match member {
                Member::Field(field) => Some(field.clone()),
                Member::Method { .. } => None,
            })
            .collect();
        let derived = members(&self.harvest.decls, id, fields.clone());
        let same = derived.len() == class.members.len()
            && derived.iter().zip(&class.members).all(|pair| match pair {
                (Member::Method { decl, sig }, Member::Method { decl: d, sig: s }) => {
                    (decl, sig) == (d, s)
                }
                (Member::Field(a), Member::Field(b)) => a.names == b.names && a.annot == b.annot,
                _ => false,
            });
        let members = match same {
            true => Members::Derived(fields.iter().map(field).collect()),
            false => Members::Written(
                class
                    .members
                    .iter()
                    .map(|member| match member {
                        Member::Field(field) => WireMember::Field(self::field(field)),
                        &Member::Method { decl, sig } => WireMember::Method(self::sig((decl, sig))),
                    })
                    .collect(),
            ),
        };
        Class {
            binders: self.binders(&class.binders),
            supers: class
                .supers
                .iter()
                .map(|supertype| Super {
                    head: opt(supertype.head.link.map(LinkId::index)),
                    args: supertype.args.iter().map(|arg| self.arg(arg)).collect(),
                    type_only: supertype.type_only,
                })
                .collect(),
            members,
            decorators: decorators(&class.decorators),
        }
    }

    fn binders(&mut self, binders: &[surface::Binder]) -> Vec<Binder> {
        binders
            .iter()
            .map(|binder| Binder {
                kind: match binder.kind {
                    surface::BinderKind::Pos => BinderKind::Pos,
                    surface::BinderKind::Key => BinderKind::Key,
                    surface::BinderKind::Rest(kind) => BinderKind::Rest(rest(kind)),
                },
                name: name(binder.name),
                bound: opt(binder.bound.map(SiteId::index)),
                default: opt(binder.default.map(SiteId::index)),
            })
            .collect()
    }

    fn signature(&mut self, sig: &surface::Signature) -> Signature {
        Signature {
            params: sig
                .params
                .iter()
                .map(|param| Param {
                    kind: match &param.kind {
                        ParamKind::Pos => WireParamKind::Pos,
                        // A key is mostly the parameter's name
                        ParamKind::Key { key } => {
                            WireParamKind::Key((param.name != Some(*key)).then(|| name(*key)))
                        }
                        ParamKind::ConstKey { key } => {
                            WireParamKind::ConstKey(key.as_ref().map(constant))
                        }
                        &ParamKind::Rest { kind, pattern } => WireParamKind::Rest {
                            kind: rest(kind),
                            pattern,
                        },
                    },
                    name: param.name.map(name),
                    default: param.default,
                    annot: opt(param.annot.map(SiteId::index)),
                })
                .collect(),
            input: opt(sig.input.map(SiteId::index)),
            output: opt(sig.output.map(SiteId::index)),
            ret: opt(sig.ret.map(SiteId::index)),
        }
    }

    fn site(&mut self, site: &elab::Site, placed: Placed) -> Site {
        let ambient = match site.ambient {
            None => Ambient::None,
            found if found == self::ambient(&self.harvest.decls, site) => Ambient::Group,
            Some(found) => Ambient::Sig(sig(found)),
        };
        Site {
            ty: self.ty(&site.ty),
            role: match site.role {
                Role::Type => WireRole::Type,
                Role::Rest => WireRole::Rest,
                Role::Pattern => WireRole::Pattern,
                Role::Bound(found) => WireRole::Bound(binder(found)),
                Role::Default(found) => WireRole::Default(binder(found)),
                Role::Alias(decl) => WireRole::Alias(index(decl.index())),
            },
            owner: (placed != Some(site.owner)).then(|| site.owner.map(sig)),
            ambient,
        }
    }

    fn arg(&mut self, arg: &surface::TypeArg) -> Arg {
        match &arg.kind {
            TypeArgKind::Pos(ty) => Arg::Pos(self.ty(ty)),
            TypeArgKind::Key { name, ty } => Arg::Key {
                name: index(name.text.index()),
                ty: self.ty(ty),
            },
            TypeArgKind::Expand { ty } => Arg::Expand(self.ty(ty)),
        }
    }

    fn params(&mut self, params: &[TypeParam]) -> Vec<WireParam> {
        params
            .iter()
            .map(|param| match param {
                TypeParam::Pos { quant, ty } => WireParam::Pos {
                    quant: quant.map(self::quant),
                    ty: self.ty(ty),
                },
                TypeParam::Key { quant, key, ty } => WireParam::Key {
                    quant: quant.map(self::quant),
                    key: match key {
                        TypeKey::Sym(symbol) => Key::Sym(index(symbol.text.index())),
                        TypeKey::Type(ty) => Key::Type(self.ty(ty)),
                    },
                    ty: self.ty(ty),
                },
                TypeParam::AnyPos => WireParam::AnyPos,
                TypeParam::AnyKey(ty) => {
                    WireParam::AnyKey(ty.as_ref().map_or(0, |ty| self.ty(ty) + 1))
                }
                TypeParam::Open(_) => WireParam::Open,
            })
            .collect()
    }

    /// The node a type is interned as, after its children
    fn ty(&mut self, ty: &TypeExpr) -> u32 {
        let node = match ty {
            TypeExpr::Name(head) => Type::Name(opt(head.link.map(LinkId::index))),
            TypeExpr::Const { value, .. } => Type::Const(value.as_ref().map(constant)),
            TypeExpr::App { base, args, .. } => Type::App {
                base: self.ty(base),
                args: args.iter().map(|arg| self.arg(arg)).collect(),
            },
            TypeExpr::Schema { params, tuple, .. } => Type::Schema {
                params: self.params(params),
                tuple: *tuple,
            },
            TypeExpr::Group { ty, .. } => Type::Group(self.ty(ty)),
            TypeExpr::Tuple { params, .. } => Type::Tuple(self.params(params)),
            TypeExpr::Record { params, .. } => Type::Record(self.params(params)),
            TypeExpr::Array { elem, .. } => Type::Array(self.ty(elem)),
            TypeExpr::Union { members, .. } => {
                Type::Union(members.iter().map(|member| self.ty(member)).collect())
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => Type::Func {
                params: self.params(params),
                input: input.as_ref().map_or(0, |ty| self.ty(ty) + 1),
                output: output.as_ref().map_or(0, |ty| self.ty(ty) + 1),
                ret: self.ty(ret),
            },
            TypeExpr::Error { .. } => Type::Error,
        };
        if let Some(&id) = self.interned.get(&node) {
            return id;
        }
        let id = index(self.types.len());
        self.types.push(node.clone());
        self.interned.insert(node, id);
        id
    }
}

fn name(name: surface::Name) -> Name {
    Name {
        span: span(name.span),
        text: index(name.text.index()),
    }
}

fn field(field: &surface::Field) -> Field {
    let mut flags = scope_flags(field.scope);
    if field.public {
        flags |= PUBLIC;
    }
    Field {
        names: field.names.iter().copied().map(name).collect(),
        annot: opt(field.annot.map(SiteId::index)),
        flags,
    }
}

fn decorators(decorators: &[Decorator]) -> Vec<WireDecorator> {
    decorators
        .iter()
        .map(|decorator| match decorator {
            Decorator::Ident(head) => WireDecorator::Ident(opt(head.link.map(LinkId::index))),
            Decorator::Other => WireDecorator::Other,
        })
        .collect()
}

fn constant(value: &ConstLit) -> Const {
    match value {
        ConstLit::Str(text) => Const::Str(index(text.index())),
        &ConstLit::Int(value) => Const::Int(value),
        &ConstLit::Bool(value) => Const::Bool(value),
        ConstLit::Nil => Const::Nil,
        ConstLit::Sym(symbol) => Const::Sym(index(symbol.text.index())),
    }
}

fn quant(quant: TypeQuant) -> Quant {
    match quant {
        TypeQuant::Opt => Quant::Opt,
        TypeQuant::Star => Quant::Star,
        TypeQuant::Include => Quant::Include,
    }
}

fn target(target: elab::Target) -> WireTarget {
    match target {
        elab::Target::Local(Referent::Decl(decl)) => WireTarget::Decl(index(decl.index())),
        elab::Target::Local(Referent::Binder(found)) => WireTarget::Binder(binder(found)),
        elab::Target::Local(Referent::Value(value)) => {
            debug_assert_eq!(value.unit, local(), "a harvest refers to its own unit");
            WireTarget::Value(span(value.span))
        }
        elab::Target::Local(Referent::Error) => WireTarget::Error,
        elab::Target::Local(Referent::Module(_)) => {
            unreachable!("a harvest names a module only once linked")
        }
        elab::Target::Import { module, item } => WireTarget::Import {
            module: index(module.index()),
            item: index(item.index()),
        },
        elab::Target::Module(module) => WireTarget::Module(index(module.index())),
    }
}

/// Decode content into a harvest. IDs are checked only where decoding follows
/// them; the harvest is validated afterwards.
pub(super) fn decode(content: Content<&str>) -> Result<Harvest<'_>, &'static str> {
    let Content {
        module,
        path,
        lines,
        strict,
        strings,
        types,
        decls,
        sites,
        links,
        exports,
        values,
        modules,
    } = content;
    let (Some(&module), Some(&path)) = (strings.get(module as usize), strings.get(path as usize))
    else {
        return Err("an ID is out of range");
    };
    // A node's children are before it
    for (index, ty) in types.iter().enumerate() {
        let mut children = Vec::new();
        ty.children(&mut children);
        if children.iter().any(|&child| child as usize >= index) {
            return Err("a type refers to one after it");
        }
    }
    let decode = Decode { types: &types };

    let mut derived = Vec::new();
    let mut harvest_decls = Vec::with_capacity(decls.len());
    for (index, decl) in decls.into_iter().enumerate() {
        let own = unname(decl.name);
        let (kind, node) = match decl.node {
            Node::Class(class) => (DeclKind::Class, decode.class(index, class, &mut derived)?),
            Node::Protocol(class) => (
                DeclKind::Protocol,
                decode.class(index, class, &mut derived)?,
            ),
            Node::Alias { binders, body } => {
                let kind = match body {
                    0 => DeclKind::OpaqueAlias,
                    _ => DeclKind::Alias,
                };
                let alias = surface::Alias {
                    binders: decode.binders(binders),
                    body: unopt(body).map(SiteId::from_index),
                };
                (kind, DeclNode::Alias(alias))
            }
            Node::Defs(defs) => {
                let defs = defs
                    .into_iter()
                    .map(|def| {
                        Ok(surface::Def {
                            name: def.name.map_or(own, unname),
                            binders: decode.binders(def.binders),
                            sig: decode.signature(def.sig)?,
                            type_only: def.type_only,
                        })
                    })
                    .collect::<Result<_, _>>()?;
                (DeclKind::Function, DeclNode::Defs(defs))
            }
            Node::Methods(methods) => {
                let methods = methods
                    .into_iter()
                    .map(|method| {
                        Ok(surface::Method {
                            name: method.name.map_or(own, unname),
                            binders: decode.binders(method.binders),
                            sig: decode.signature(method.sig)?,
                            overload: method.flags & OVERLOAD != 0,
                            special: method.special,
                            public: method.flags & PUBLIC != 0,
                            scope: scope(method.flags, OVERLOAD | PUBLIC)?,
                            decorators: undecorators(method.decorators),
                        })
                    })
                    .collect::<Result<_, _>>()?;
                (DeclKind::Function, DeclNode::Methods(methods))
            }
        };
        harvest_decls.push(elab::Decl {
            unit: local(),
            kind,
            name: Some(own),
            node,
            outer: decl.outer.map(unsig),
            ast: None,
        });
    }
    for (index, fields) in derived {
        let members = members(&harvest_decls, DeclId::from_index(index), fields);
        if let DeclNode::Class(class) = &mut harvest_decls[index].node {
            class.members = members;
        }
    }

    let values: Vec<_> = values
        .into_iter()
        .map(|value| elab::Value {
            span: unspan(value.span),
            annot: unopt(value.annot).map(SiteId::from_index),
        })
        .collect();
    let placed = placed(&harvest_decls, &values, sites.len());
    let mut harvest_sites = Vec::with_capacity(sites.len());
    for (site, placed) in sites.into_iter().zip(placed) {
        let owner = match site.owner {
            Some(owner) => owner.map(unsig),
            None => placed.ok_or("a site is not on the surface")?,
        };
        let mut decoded = elab::Site {
            unit: local(),
            ty: decode.ty(site.ty)?,
            role: match site.role {
                WireRole::Type => Role::Type,
                WireRole::Rest => Role::Rest,
                WireRole::Pattern => Role::Pattern,
                WireRole::Bound(found) => Role::Bound(unbinder(found)),
                WireRole::Default(found) => Role::Default(unbinder(found)),
                WireRole::Alias(decl) => Role::Alias(DeclId::from_index(decl as usize)),
            },
            ambient: None,
            owner,
            interned: Default::default(),
        };
        decoded.ambient = match site.ambient {
            Ambient::None => None,
            Ambient::Group => ambient(&harvest_decls, &decoded),
            Ambient::Sig(found) => Some(unsig(found)),
        };
        harvest_sites.push(decoded);
    }

    Ok(Harvest {
        info: UnitInfo {
            module: Some(module),
            path: Path::new(path),
            lines,
            source: None,
            strict,
        },
        strings: strings.into_iter().map(Cow::Borrowed).collect(),
        decls: harvest_decls,
        sites: harvest_sites,
        links: links
            .into_iter()
            .map(|link| elab::Link {
                unit: local(),
                base: untarget(link.base),
                item: unopt(link.item).map(StrId::from_index),
                span: Span::INVALID,
            })
            .collect(),
        refs: FxHashMap::default(),
        exports: exports
            .into_iter()
            .map(|export| elab::Export {
                name: StrId::from_index(export.name as usize),
                span: unspan(export.span),
                target: untarget(export.target),
            })
            .collect(),
        values,
        modules: modules
            .into_iter()
            .map(|module| elab::Module {
                name: StrId::from_index(module.name as usize),
                span: unspan(module.span),
            })
            .collect(),
    })
}

/// Builds a harvest's surface from its serialization form
struct Decode<'t> {
    types: &'t [Type],
}

impl Decode<'_> {
    /// A class, whose members are left for `derived` where they are derived
    fn class(
        &self,
        index: usize,
        class: Class,
        derived: &mut Vec<(usize, Vec<surface::Field>)>,
    ) -> Result<DeclNode, &'static str> {
        let members = match class.members {
            Members::Derived(fields) => {
                let fields = fields.into_iter().map(unfield).collect::<Result<_, _>>()?;
                derived.push((index, fields));
                Vec::new()
            }
            Members::Written(members) => members
                .into_iter()
                .map(|member| match member {
                    WireMember::Field(field) => unfield(field).map(Member::Field),
                    WireMember::Method(found) => {
                        let (decl, sig) = unsig(found);
                        Ok(Member::Method { decl, sig })
                    }
                })
                .collect::<Result<_, _>>()?,
        };
        let supers = class
            .supers
            .into_iter()
            .map(|supertype| {
                Ok(surface::Super {
                    head: unref(supertype.head),
                    args: self.args(supertype.args)?,
                    bracket_span: None,
                    type_only: supertype.type_only,
                    interned: Default::default(),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(DeclNode::Class(surface::Class {
            binders: self.binders(class.binders),
            supers,
            members,
            decorators: undecorators(class.decorators),
        }))
    }

    fn binders(&self, binders: Vec<Binder>) -> Vec<surface::Binder> {
        binders
            .into_iter()
            .map(|binder| surface::Binder {
                kind: match binder.kind {
                    BinderKind::Pos => surface::BinderKind::Pos,
                    BinderKind::Key => surface::BinderKind::Key,
                    BinderKind::Rest(kind) => surface::BinderKind::Rest(unrest(kind)),
                },
                name: unname(binder.name),
                bound: unopt(binder.bound).map(SiteId::from_index),
                default: unopt(binder.default).map(SiteId::from_index),
            })
            .collect()
    }

    fn signature(&self, sig: Signature) -> Result<surface::Signature, &'static str> {
        Ok(surface::Signature {
            params: sig
                .params
                .into_iter()
                .map(|param| {
                    let name = param.name.map(unname);
                    Ok(surface::Param {
                        kind: match param.kind {
                            WireParamKind::Pos => ParamKind::Pos,
                            WireParamKind::Key(key) => ParamKind::Key {
                                key: match key {
                                    Some(key) => unname(key),
                                    None => name.ok_or("a key is the name of no parameter")?,
                                },
                            },
                            WireParamKind::ConstKey(key) => ParamKind::ConstKey {
                                key: key.map(unconstant),
                            },
                            WireParamKind::Rest { kind, pattern } => ParamKind::Rest {
                                kind: unrest(kind),
                                pattern,
                            },
                        },
                        name,
                        default: param.default,
                        annot: unopt(param.annot).map(SiteId::from_index),
                    })
                })
                .collect::<Result<_, _>>()?,
            input: unopt(sig.input).map(SiteId::from_index),
            output: unopt(sig.output).map(SiteId::from_index),
            ret: unopt(sig.ret).map(SiteId::from_index),
        })
    }

    fn args(&self, args: Vec<Arg>) -> Result<Vec<surface::TypeArg>, &'static str> {
        args.into_iter()
            .map(|arg| {
                let kind = match arg {
                    Arg::Pos(ty) => TypeArgKind::Pos(self.ty(ty)?),
                    Arg::Key { name, ty } => TypeArgKind::Key {
                        name: symbol(name),
                        ty: self.ty(ty)?,
                    },
                    Arg::Expand(ty) => TypeArgKind::Expand { ty: self.ty(ty)? },
                };
                Ok(surface::TypeArg { kind })
            })
            .collect()
    }

    fn params(&self, params: &[WireParam]) -> Result<Vec<TypeParam>, &'static str> {
        params
            .iter()
            .map(|param| {
                Ok(match param {
                    &WireParam::Pos { quant, ty } => TypeParam::Pos {
                        quant: quant.map(unquant),
                        ty: self.ty(ty)?,
                    },
                    WireParam::Key { quant, key, ty } => TypeParam::Key {
                        quant: quant.map(unquant),
                        key: match *key {
                            Key::Sym(text) => TypeKey::Sym(symbol(text)),
                            Key::Type(ty) => TypeKey::Type(Box::new(self.ty(ty)?)),
                        },
                        ty: self.ty(*ty)?,
                    },
                    WireParam::AnyPos => TypeParam::AnyPos,
                    &WireParam::AnyKey(ty) => TypeParam::AnyKey(self.opt_ty(ty)?),
                    WireParam::Open => TypeParam::Open(Span::INVALID),
                })
            })
            .collect()
    }

    fn opt_ty(&self, ty: Opt) -> Result<Option<TypeExpr>, &'static str> {
        ty.checked_sub(1).map(|ty| self.ty(ty)).transpose()
    }

    /// A fresh tree of a node, since each place a type is written in has slots of
    /// its own
    fn ty(&self, id: u32) -> Result<TypeExpr, &'static str> {
        let span = Span::INVALID;
        let boxed = |id| self.ty(id).map(Box::new);
        Ok(
            match self.types.get(id as usize).ok_or("an ID is out of range")? {
                &Type::Name(link) => TypeExpr::Name(unref(link)),
                Type::Const(value) => TypeExpr::Const {
                    span,
                    value: value.clone().map(unconstant),
                },
                Type::App { base, args } => TypeExpr::App {
                    span,
                    base: boxed(*base)?,
                    args: self.args(args.clone())?,
                    interned: Default::default(),
                },
                Type::Schema { params, tuple } => TypeExpr::Schema {
                    span,
                    params: self.params(params)?,
                    tuple: *tuple,
                    collection: Default::default(),
                },
                &Type::Group(ty) => TypeExpr::Group {
                    span,
                    ty: boxed(ty)?,
                },
                Type::Tuple(params) => TypeExpr::Tuple {
                    span,
                    params: self.params(params)?,
                },
                Type::Record(params) => TypeExpr::Record {
                    span,
                    params: self.params(params)?,
                },
                &Type::Array(elem) => TypeExpr::Array {
                    span,
                    elem: boxed(elem)?,
                },
                Type::Union(members) => TypeExpr::Union {
                    span,
                    members: members
                        .iter()
                        .map(|&member| self.ty(member))
                        .collect::<Result<_, _>>()?,
                },
                Type::Func {
                    params,
                    input,
                    output,
                    ret,
                } => TypeExpr::Func {
                    span,
                    params: self.params(params)?,
                    input: self.opt_ty(*input)?.map(Box::new),
                    output: self.opt_ty(*output)?.map(Box::new),
                    ret: boxed(*ret)?,
                    interned: Default::default(),
                },
                Type::Error => TypeExpr::Error { span },
            },
        )
    }
}

impl Type {
    /// The nodes a node refers to
    fn children(&self, out: &mut Vec<u32>) {
        let params = |out: &mut Vec<u32>, params: &[WireParam]| {
            for param in params {
                match param {
                    WireParam::Pos { ty, .. } => out.push(*ty),
                    WireParam::Key { key, ty, .. } => {
                        if let Key::Type(key) = key {
                            out.push(*key);
                        }
                        out.push(*ty);
                    }
                    WireParam::AnyKey(ty) => out.extend(ty.checked_sub(1)),
                    WireParam::AnyPos | WireParam::Open => {}
                }
            }
        };
        match self {
            Type::Name(_) | Type::Const(_) | Type::Error => {}
            Type::App { base, args } => {
                out.push(*base);
                out.extend(args.iter().map(|arg| match *arg {
                    Arg::Pos(ty) | Arg::Key { ty, .. } | Arg::Expand(ty) => ty,
                }));
            }
            Type::Schema { params: items, .. } | Type::Tuple(items) | Type::Record(items) => {
                params(out, items)
            }
            Type::Group(ty) | Type::Array(ty) => out.push(*ty),
            Type::Union(members) => out.extend(members),
            Type::Func {
                params: items,
                input,
                output,
                ret,
            } => {
                params(out, items);
                out.extend(input.checked_sub(1));
                out.extend(output.checked_sub(1));
                out.push(*ret);
            }
        }
    }
}

fn unopt(id: Opt) -> Option<usize> {
    id.checked_sub(1).map(|id| id as usize)
}

fn unsig((decl, sig): Sig) -> (DeclId, usize) {
    (DeclId::from_index(decl as usize), sig as usize)
}

fn unbinder((decl, sig, slot): WireBinder) -> BinderRef {
    BinderRef {
        decl: DeclId::from_index(decl as usize),
        sig: sig as usize,
        slot: slot as usize,
    }
}

fn unname(name: Name) -> surface::Name {
    surface::Name {
        span: unspan(name.span),
        text: StrId::from_index(name.text as usize),
    }
}

fn symbol(text: u32) -> Symbol {
    Symbol {
        span: Span::INVALID,
        text: StrId::from_index(text as usize),
    }
}

fn unref(link: Opt) -> Ref {
    Ref {
        span: Span::INVALID,
        link: unopt(link).map(LinkId::from_index),
    }
}

fn unfield(field: Field) -> Result<surface::Field, &'static str> {
    Ok(surface::Field {
        names: field.names.into_iter().map(unname).collect(),
        annot: unopt(field.annot).map(SiteId::from_index),
        public: field.flags & PUBLIC != 0,
        scope: scope(field.flags, PUBLIC)?,
    })
}

fn undecorators(decorators: Vec<WireDecorator>) -> Vec<Decorator> {
    decorators
        .into_iter()
        .map(|decorator| match decorator {
            WireDecorator::Ident(link) => Decorator::Ident(unref(link)),
            WireDecorator::Other => Decorator::Other,
        })
        .collect()
}

fn unconstant(value: Const) -> ConstLit {
    match value {
        Const::Str(text) => ConstLit::Str(StrId::from_index(text as usize)),
        Const::Int(value) => ConstLit::Int(value),
        Const::Bool(value) => ConstLit::Bool(value),
        Const::Nil => ConstLit::Nil,
        Const::Sym(text) => ConstLit::Sym(symbol(text)),
    }
}

fn unquant(quant: Quant) -> TypeQuant {
    match quant {
        Quant::Opt => TypeQuant::Opt,
        Quant::Star => TypeQuant::Star,
        Quant::Include => TypeQuant::Include,
    }
}

fn untarget(target: WireTarget) -> elab::Target {
    match target {
        WireTarget::Decl(decl) => {
            elab::Target::Local(Referent::Decl(DeclId::from_index(decl as usize)))
        }
        WireTarget::Binder(found) => elab::Target::Local(Referent::Binder(unbinder(found))),
        WireTarget::Value(span) => elab::Target::Local(Referent::Value(UnitSpan {
            unit: local(),
            span: unspan(span),
        })),
        WireTarget::Error => elab::Target::Local(Referent::Error),
        WireTarget::Import { module, item } => elab::Target::Import {
            module: ModuleId::from_index(module as usize),
            item: StrId::from_index(item as usize),
        },
        WireTarget::Module(module) => elab::Target::Module(ModuleId::from_index(module as usize)),
    }
}

#[cfg(test)]
mod tests {
    use super::{MemberScope, OVERLOAD, PUBLIC, SCOPE_SHIFT, scope};

    #[test]
    fn unknown_flags() {
        assert!(matches!(
            scope(PUBLIC | 1 << SCOPE_SHIFT, PUBLIC),
            Ok(MemberScope::Class)
        ));
        // A field is never an overload, and no scope is 3
        assert!(scope(OVERLOAD, PUBLIC).is_err());
        assert!(scope(3 << SCOPE_SHIFT, PUBLIC | OVERLOAD).is_err());
    }
}
