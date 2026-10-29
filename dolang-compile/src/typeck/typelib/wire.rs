//! A typelib's serialization form, and its conversion to and from a shard.
//!
//! The form follows the shard, cut down to what its surface keeps, except that it
//! is denser:
//!
//! - Type expressions are interned in one table, each distinct node once, with its
//!   children before it. A site, or a supertype's argument, refers to a node.
//! - What can be derived from the rest is left out where it holds: a site's owner
//!   and ambient signature, a def's or method's name where it is its declaration's,
//!   a parameter's key where it is the parameter's name, and a class's methods
//!   among its members. Where a derivation doesn't hold, the value is written.
//! - A declaration's kind is its node's, flags are packed into a byte, and an
//!   optional ID is written as one more than the ID, or 0.
//!
//! Every ID is local to the unit. Declarations, sites and links keep their order;
//! strings are numbered as the encoder first meets them, and each is written once.

use std::{borrow::Cow, mem, path::Path};

use bitvec::slice::BitSlice;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::{
    RestKind,
    ast::SpecialMethod,
    source::{self, Span},
    typeck::{
        elab::{
            self, Bases, BinderRef, DeclNode, Referent, Role, Shard, UnitInfo, local,
            surface::{
                self, ConstLit, Decorator, LinkId, Member, MemberScope, ModuleId, ParamKind, Ref,
                SiteId, StrId, Symbol, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeQuant,
            },
        },
        r#type::{DeclId, DeclKind, UnitSpan, Variance},
    },
};

/// A typelib's content, its strings either borrowed from the bytes it was read
/// from or owned by the check that keeps it
#[derive(Serialize, Deserialize)]
pub(super) struct Content<S> {
    pub(super) module: u32,
    /// The module's path, which only locates its diagnostics
    pub(super) path: u32,
    /// The number and start of each line holding a span the typelib keeps. Spans
    /// and line starts are offsets in characters, which locates them in characters
    /// without the source.
    pub(super) lines: Vec<(u32, u32)>,
    /// Whether the module was checked strictly, which its signatures depend on
    pub(super) strict: bool,
    pub(super) strings: Vec<S>,
    /// Each distinct type expression node, after its children
    pub(super) types: Vec<Type>,
    decls: Vec<Decl>,
    /// What elaboration concluded about each declaration, in the same order
    pub(super) facts: Vec<DeclFacts>,
    sites: Vec<Site>,
    links: Vec<Link>,
    exports: Vec<Export>,
    values: Vec<Value>,
    modules: Vec<Module>,
    /// The types `strand`'s pipe placeholders stand for in the check that wrote
    /// the typelib, each as its module and item, where the facts can depend on
    /// them
    pipes: Option<[(u32, u32); 2]>,
}

/// A span's start and end, in characters
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
/// What elaboration concluded about a declaration, which a check loading the
/// typelib takes as given
#[derive(Serialize, Deserialize)]
pub(super) struct DeclFacts {
    /// The variance of each signature's binders, by slot: its written binders,
    /// then the implicit binder of each ambient channel it omits
    pub(super) variance: Vec<Vec<WireVariance>>,
    /// The variance of each outer binder the declaration uses, by binder
    pub(super) captured: Vec<(WireBinder, WireVariance)>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum WireVariance {
    Invariant,
    Covariant,
    Contravariant,
}

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
pub(super) type WireBinder = (u32, u32, u32);

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

fn unspan((start, end): WireSpan) -> Span {
    Span { start, end }
}

fn variance(variance: Variance) -> WireVariance {
    match variance {
        Variance::Invariant => WireVariance::Invariant,
        Variance::Covariant => WireVariance::Covariant,
        Variance::Contravariant => WireVariance::Contravariant,
    }
}

fn unvariance(variance: WireVariance) -> Variance {
    match variance {
        WireVariance::Invariant => Variance::Invariant,
        WireVariance::Covariant => Variance::Covariant,
        WireVariance::Contravariant => Variance::Contravariant,
    }
}

/// The number of binders of a declaration's signature `sig`: its written binders,
/// then the implicit binder of each ambient channel a def or method omits. `None`
/// for a signature it doesn't have.
fn slots(node: &DeclNode, sig: usize) -> Option<usize> {
    let channels = |sig: &surface::Signature| {
        usize::from(sig.input.is_none()) + usize::from(sig.output.is_none())
    };
    match node {
        DeclNode::Class(class) if sig == 0 => Some(class.binders.len()),
        DeclNode::Alias(alias) if sig == 0 => Some(alias.binders.len()),
        DeclNode::Defs(defs) => defs
            .get(sig)
            .map(|def| def.binders.len() + channels(&def.sig)),
        DeclNode::Methods(methods) => {
            (methods.get(sig)).map(|method| method.binders.len() + channels(&method.sig))
        }
        _ => None,
    }
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

/// The ambient signature a site's place gives it, given the declarations by ID
fn ambient<'d>(
    decl: impl Fn(DeclId) -> Option<&'d elab::Decl<'d>>,
    site: &elab::Site,
) -> Option<(DeclId, usize)> {
    site.group().filter(|(id, _)| {
        decl(*id).is_some_and(|decl| matches!(decl.node, DeclNode::Defs(_) | DeclNode::Methods(_)))
    })
}

/// The members of a class with fields `fields`, among declarations `decls` by ID:
/// they and the method signatures declared in it, in the order of the spans of
/// their names
fn members<'d>(
    decls: impl Iterator<Item = (DeclId, &'d elab::Decl<'d>)>,
    class: DeclId,
    fields: Vec<surface::Field>,
) -> Vec<Member> {
    let mut members: Vec<(u32, Member)> = fields
        .into_iter()
        .map(|field| {
            let start = field.names.first().map_or(0, |name| name.span.start);
            (start, Member::Field(field))
        })
        .collect();
    for (decl, node) in decls {
        if node.outer != Some((class, 0)) {
            continue;
        }
        let DeclNode::Methods(methods) = &node.node else {
            continue;
        };
        for (sig, method) in methods.iter().enumerate() {
            members.push((method.name.span.start, Member::Method { decl, sig }));
        }
    }
    members.sort_by_key(|(start, _)| *start);
    members.into_iter().map(|(_, member)| member).collect()
}

/// Converts what a shard's surface keeps to its serialization form. The shard's
/// IDs start at its bases; the form's are local to the unit, its declarations,
/// sites and links numbered in their order and its strings as they are first met.
pub(super) struct Encode<'s, 'u> {
    shard: &'s Shard<'u>,
    base: &'s Bases,
    kept: &'s BitSlice,
    facts: &'s elab::Facts,
    pipes: Option<[(&'s str, &'s str); 2]>,
    /// The ID of each kept declaration, by its offset in the unit
    decls: Vec<Option<u32>>,
    /// The ID of each site met, by its offset in the unit
    site_ids: Vec<Option<u32>>,
    /// Each site met, in order, with the owner its place gives it
    sites: Vec<(SiteId, Option<(DeclId, usize)>)>,
    /// The ID of each link met, by its offset in the unit
    link_ids: Vec<Option<u32>>,
    /// Each link met, in order
    links: Vec<LinkId>,
    /// The ID of each string met, by its [`StrId`]
    str_ids: Vec<Option<u32>>,
    /// Each string, by text
    texts: FxHashMap<String, u32>,
    strings: Vec<String>,
    types: Vec<Type>,
    interned: FxHashMap<Type, u32>,
    /// The offset in characters of each line's start, for a shard with source,
    /// whose spans are in bytes
    chars: Option<Vec<u32>>,
    /// Every span written, as the shard has it
    spans: Vec<Span>,
    /// The number and start of each line holding a span written
    lines: Vec<(u32, u32)>,
}

impl<'s, 'u> Encode<'s, 'u> {
    /// An encoder of what `kept` keeps of a shard whose IDs start at `base`, with
    /// the facts concluded about it and the pipe nominations they depend on, if any
    pub(super) fn new(
        shard: &'s Shard<'u>,
        base: &'s Bases,
        kept: &'s BitSlice,
        facts: &'s elab::Facts,
        pipes: Option<[(&'s str, &'s str); 2]>,
    ) -> Self {
        let mut next = 0;
        let decls = kept
            .iter()
            .by_vals()
            .map(|keep| {
                keep.then(|| {
                    next += 1;
                    next - 1
                })
            })
            .collect();
        Self {
            shard,
            base,
            kept,
            facts,
            pipes,
            decls,
            site_ids: vec![None; shard.sites.len()],
            sites: Vec::new(),
            link_ids: vec![None; shard.links.len()],
            links: Vec::new(),
            str_ids: vec![None; shard.strings.len()],
            texts: FxHashMap::default(),
            strings: Vec::new(),
            types: Vec::new(),
            interned: FxHashMap::default(),
            chars: shard
                .info
                .source
                .map(|unit| line_chars(unit.file.content())),
            spans: Vec::new(),
            lines: Vec::new(),
        }
    }

    /// The content, and every span it holds
    pub(super) fn content(mut self) -> (Content<String>, Vec<Span>) {
        // The first pass finds the sites and links the surface uses. They keep
        // their order, which elaboration's diagnostics follow, so the second
        // numbers them in it.
        self.pass();
        let (mut sites, mut links) = (mem::take(&mut self.sites), mem::take(&mut self.links));
        sites.sort_by_key(|(site, _)| site.index());
        links.sort_by_key(|link| link.index());
        self.site_ids.fill(None);
        self.link_ids.fill(None);
        for (site, owner) in sites {
            self.site(Some(site), owner);
        }
        for link in links {
            self.link(Some(link));
        }
        self.str_ids.fill(None);
        self.texts.clear();
        self.interned.clear();
        self.spans.clear();
        self.lines.clear();
        let content = self.pass();
        (content, self.spans)
    }

    /// Encode the content, numbering what is met that isn't yet numbered.
    fn pass(&mut self) -> Content<String> {
        let shard = self.shard;
        let info = &shard.info;
        let module = self.text(info.module.expect("only a module's typelib is written"));
        let path = self.text(&info.path.to_string_lossy());
        let decls = (shard.decls.iter().enumerate())
            .filter(|&(index, _)| self.kept[index])
            .map(|(index, decl)| self.decl(DeclId::from_index(self.base.decls + index), decl))
            .collect();
        let facts = self.decl_facts();
        let values = (shard.values.iter())
            .map(|value| Value {
                span: self.span(value.span),
                annot: self.site(value.annot, None),
            })
            .collect();
        let exports = (shard.exports.iter())
            .map(|export| Export {
                name: self.str(export.name),
                span: self.span(export.span),
                target: self.target(export.target),
            })
            .collect();
        let modules = (shard.modules.iter())
            .map(|module| Module {
                name: self.str(module.name),
                span: self.span(module.span),
            })
            .collect();
        let pipes = self
            .pipes
            .map(|pipes| pipes.map(|(module, item)| (self.text(module), self.text(item))));
        // A site meets links, which meet no sites
        let mut sites = Vec::new();
        while let Some(&(site, placed)) = self.sites.get(sites.len()) {
            sites.push(self.site_of(site, placed));
        }
        let mut links = Vec::new();
        while let Some(&link) = self.links.get(links.len()) {
            let link = &shard.links[link.index() - self.base.links];
            links.push(Link {
                base: self.target(link.base),
                item: opt(link.item.map(|item| self.str(item) as usize)),
            });
        }
        let mut lines = mem::take(&mut self.lines);
        lines.sort_unstable();
        lines.dedup();
        Content {
            module,
            path,
            lines,
            strict: info.strict,
            strings: mem::take(&mut self.strings),
            types: mem::take(&mut self.types),
            decls,
            facts,
            sites,
            links,
            exports,
            values,
            modules,
            pipes,
        }
    }

    /// The facts of each kept declaration
    fn decl_facts(&self) -> Vec<DeclFacts> {
        let mut captured: FxHashMap<DeclId, Vec<(WireBinder, WireVariance)>> = FxHashMap::default();
        for (&(decl, binder), &found) in &self.facts.captured {
            if self.decl_id(decl).is_some() {
                let entry = (self.binder(binder), variance(found));
                captured.entry(decl).or_default().push(entry);
            }
        }
        (self.shard.decls.iter().enumerate())
            .filter(|&(index, _)| self.kept[index])
            .map(|(index, decl)| {
                let id = DeclId::from_index(self.base.decls + index);
                let variances = (0..decl.node.sig_count())
                    .map(|sig| {
                        let slots = slots(&decl.node, sig).expect("a kept signature has binders");
                        (0..slots)
                            .map(|slot| {
                                let binder = BinderRef {
                                    decl: id,
                                    sig,
                                    slot,
                                };
                                let found = self.facts.variance.get(&binder);
                                variance(*found.expect("every binder has a variance"))
                            })
                            .collect()
                    })
                    .collect();
                let mut captured = captured.remove(&id).unwrap_or_default();
                captured.sort_unstable_by_key(|&(binder, _)| binder);
                DeclFacts {
                    variance: variances,
                    captured,
                }
            })
            .collect()
    }

    /// A declaration's offset in the unit
    fn offset(&self, decl: DeclId) -> usize {
        decl.index() - self.base.decls
    }

    /// The ID of a declaration, if it is kept
    fn decl_id(&self, decl: DeclId) -> Option<u32> {
        self.decls[self.offset(decl)]
    }

    /// The ID of a declaration the surface keeps
    fn kept_id(&self, decl: DeclId) -> u32 {
        self.decl_id(decl)
            .expect("the surface keeps what it refers to")
    }

    /// A kept declaration, by ID
    fn kept_decl(&self, decl: DeclId) -> Option<&'s elab::Decl<'u>> {
        let offset = decl.index().checked_sub(self.base.decls)?;
        self.decls.get(offset)?.and(self.shard.decls.get(offset))
    }

    fn sig(&self, (decl, sig): (DeclId, usize)) -> Sig {
        (self.kept_id(decl), index(sig))
    }

    fn binder(&self, binder: BinderRef) -> WireBinder {
        (
            self.kept_id(binder.decl),
            index(binder.sig),
            index(binder.slot),
        )
    }

    fn span(&mut self, span: Span) -> WireSpan {
        self.spans.push(span);
        if span == Span::INVALID {
            return (span.start, span.end);
        }
        let mut offset = |offset: u32| {
            let info = &self.shard.info;
            let coord = info.coord(offset);
            let (start, column) = match (&self.chars, info.source) {
                (Some(chars), Some(unit)) => {
                    let line = (offset - coord.column) as usize;
                    let prefix = &unit.file.content().as_bytes()[line..offset as usize];
                    (chars[coord.line as usize], source::chars(prefix))
                }
                _ => (offset - coord.column, coord.column),
            };
            self.lines.push((coord.line, start));
            start + column
        };
        (offset(span.start), offset(span.end))
    }

    fn name(&mut self, name: surface::Name) -> Name {
        Name {
            span: self.span(name.span),
            text: self.str(name.text),
        }
    }

    /// The ID of a text
    fn text(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.texts.get(text) {
            return id;
        }
        let id = index(self.strings.len());
        self.strings.push(text.to_owned());
        self.texts.insert(text.to_owned(), id);
        id
    }

    /// The ID of a string of the shard
    fn str(&mut self, id: StrId) -> u32 {
        if let Some(found) = self.str_ids[id.index()] {
            return found;
        }
        let shard = self.shard;
        let found = self.text(&shard.strings[id.index()]);
        self.str_ids[id.index()] = Some(found);
        found
    }

    /// The ID of a site, met where its place gives it `owner`
    fn site(&mut self, site: Option<SiteId>, owner: Option<(DeclId, usize)>) -> Opt {
        let Some(site) = site else {
            return 0;
        };
        let offset = site.index() - self.base.sites;
        let id = *self.site_ids[offset].get_or_insert_with(|| {
            self.sites.push((site, owner));
            index(self.sites.len() - 1)
        });
        id + 1
    }

    /// The ID of a link
    fn link(&mut self, link: Option<LinkId>) -> Opt {
        let Some(link) = link else {
            return 0;
        };
        let offset = link.index() - self.base.links;
        let id = *self.link_ids[offset].get_or_insert_with(|| {
            self.links.push(link);
            index(self.links.len() - 1)
        });
        id + 1
    }

    fn decl(&mut self, id: DeclId, decl: &elab::Decl<'_>) -> Decl {
        let own = decl.name.expect("a declaration on a surface is named");
        // A def's or method's name is mostly its declaration's
        let named = |this: &mut Self, name: surface::Name| (name != own).then(|| this.name(name));
        let node = match (&decl.node, decl.kind) {
            (DeclNode::Class(class), kind) => {
                let class = self.class(id, decl.outer, class);
                match kind {
                    DeclKind::Protocol => Node::Protocol(class),
                    _ => Node::Class(class),
                }
            }
            (DeclNode::Alias(alias), _) => Node::Alias {
                binders: self.binders(&alias.binders, decl.outer),
                body: self.site(alias.body, decl.outer),
            },
            (DeclNode::Defs(defs), _) => Node::Defs(
                (defs.iter().enumerate())
                    .map(|(index, def)| Def {
                        name: named(self, def.name),
                        binders: self.binders(&def.binders, decl.outer),
                        sig: self.signature(&def.sig, (id, index)),
                        type_only: def.type_only,
                    })
                    .collect(),
            ),
            (DeclNode::Methods(methods), _) => Node::Methods(
                (methods.iter().enumerate())
                    .map(|(index, method)| {
                        let mut flags = scope_flags(method.scope);
                        if method.overload {
                            flags |= OVERLOAD;
                        }
                        if method.public {
                            flags |= PUBLIC;
                        }
                        Method {
                            name: named(self, method.name),
                            flags,
                            special: method.special,
                            binders: self.binders(&method.binders, decl.outer),
                            sig: self.signature(&method.sig, (id, index)),
                            decorators: self.decorators(&method.decorators),
                        }
                    })
                    .collect(),
            ),
            (DeclNode::Closure(_), _) => unreachable!("a closure is never on a surface"),
        };
        Decl {
            name: self.name(own),
            node,
            outer: decl.outer.map(|outer| self.sig(outer)),
        }
    }

    fn class(
        &mut self,
        id: DeclId,
        outer: Option<(DeclId, usize)>,
        class: &surface::Class,
    ) -> Class {
        let kept: Vec<&Member> = (class.members.iter())
            .filter(|member| match member {
                Member::Field(field) => field.public,
                &Member::Method { decl, .. } => self.decl_id(*decl).is_some(),
            })
            .collect();
        let fields: Vec<_> = (kept.iter())
            .filter_map(|member| match member {
                Member::Field(field) => Some(field.clone()),
                Member::Method { .. } => None,
            })
            .collect();
        let shard = self.shard;
        let decls = (shard.decls.iter().enumerate())
            .filter(|&(index, _)| self.kept[index])
            .map(|(index, decl)| (DeclId::from_index(self.base.decls + index), decl));
        let derived = members(decls, id, fields.clone());
        let same = derived.len() == kept.len()
            && derived.iter().zip(&kept).all(|pair| match pair {
                (Member::Method { decl, sig }, Member::Method { decl: d, sig: s }) => {
                    (decl, sig) == (d, s)
                }
                (Member::Field(a), Member::Field(b)) => a.names == b.names && a.annot == b.annot,
                _ => false,
            });
        let members = match same {
            true => Members::Derived(fields.iter().map(|field| self.field(id, field)).collect()),
            false => Members::Written(
                kept.iter()
                    .map(|member| match member {
                        Member::Field(field) => WireMember::Field(self.field(id, field)),
                        &&Member::Method { decl, sig } => WireMember::Method(self.sig((decl, sig))),
                    })
                    .collect(),
            ),
        };
        Class {
            binders: self.binders(&class.binders, outer),
            supers: class
                .supers
                .iter()
                .map(|supertype| Super {
                    head: self.link(supertype.head.link),
                    args: supertype.args.iter().map(|arg| self.arg(arg)).collect(),
                    type_only: supertype.type_only,
                })
                .collect(),
            members,
            decorators: self.decorators(&class.decorators),
        }
    }

    fn field(&mut self, class: DeclId, field: &surface::Field) -> Field {
        let mut flags = scope_flags(field.scope);
        if field.public {
            flags |= PUBLIC;
        }
        Field {
            names: field.names.iter().map(|&name| self.name(name)).collect(),
            annot: self.site(field.annot, Some((class, 0))),
            flags,
        }
    }

    /// A declaration's binders, whose bounds and defaults are written where it is
    fn binders(
        &mut self,
        binders: &[surface::Binder],
        outer: Option<(DeclId, usize)>,
    ) -> Vec<Binder> {
        binders
            .iter()
            .map(|binder| Binder {
                kind: match binder.kind {
                    surface::BinderKind::Pos => BinderKind::Pos,
                    surface::BinderKind::Key => BinderKind::Key,
                    surface::BinderKind::Rest(kind) => BinderKind::Rest(rest(kind)),
                },
                name: self.name(binder.name),
                bound: self.site(binder.bound, outer),
                default: self.site(binder.default, outer),
            })
            .collect()
    }

    /// Signature `owner` of a def or method
    fn signature(&mut self, sig: &surface::Signature, owner: (DeclId, usize)) -> Signature {
        Signature {
            params: sig
                .params
                .iter()
                .map(|param| Param {
                    kind: match &param.kind {
                        ParamKind::Pos => WireParamKind::Pos,
                        // A key is mostly the parameter's name
                        ParamKind::Key { key } => {
                            WireParamKind::Key((param.name != Some(*key)).then(|| self.name(*key)))
                        }
                        ParamKind::ConstKey { key } => {
                            WireParamKind::ConstKey(key.as_ref().map(|key| self.constant(key)))
                        }
                        &ParamKind::Rest { kind, pattern } => WireParamKind::Rest {
                            kind: rest(kind),
                            pattern,
                        },
                    },
                    name: param.name.map(|name| self.name(name)),
                    default: param.default,
                    annot: self.site(param.annot, Some(owner)),
                })
                .collect(),
            input: self.site(sig.input, Some(owner)),
            output: self.site(sig.output, Some(owner)),
            ret: self.site(sig.ret, Some(owner)),
        }
    }

    /// A site, whose place gives it `placed` as its owner
    fn site_of(&mut self, site: SiteId, placed: Option<(DeclId, usize)>) -> Site {
        let shard = self.shard;
        let site = &shard.sites[site.index() - self.base.sites];
        let ambient = match site.ambient {
            None => Ambient::None,
            found if found == self::ambient(|decl| self.kept_decl(decl), site) => Ambient::Group,
            Some(found) => Ambient::Sig(self.sig(found)),
        };
        Site {
            ty: self.ty(&site.ty),
            role: match site.role {
                Role::Type => WireRole::Type,
                Role::Rest => WireRole::Rest,
                Role::Pattern => WireRole::Pattern,
                Role::Bound(found) => WireRole::Bound(self.binder(found)),
                Role::Default(found) => WireRole::Default(self.binder(found)),
                Role::Alias(decl) => WireRole::Alias(self.kept_id(decl)),
            },
            owner: (placed != site.owner).then(|| site.owner.map(|owner| self.sig(owner))),
            ambient,
        }
    }

    fn decorators(&mut self, decorators: &[Decorator]) -> Vec<WireDecorator> {
        decorators
            .iter()
            .map(|decorator| match decorator {
                // A decorator naming a declaration left out names nothing
                Decorator::Ident(head) => {
                    WireDecorator::Ident(self.link(head.link.filter(|link| {
                        match self.shard.links[link.index() - self.base.links].base {
                            elab::Target::Local(Referent::Decl(decl)) => {
                                self.decl_id(decl).is_some()
                            }
                            _ => true,
                        }
                    })))
                }
                Decorator::Other => WireDecorator::Other,
            })
            .collect()
    }

    fn constant(&mut self, value: &ConstLit) -> Const {
        match value {
            &ConstLit::Str(text) => Const::Str(self.str(text)),
            &ConstLit::Int(value) => Const::Int(value),
            &ConstLit::Bool(value) => Const::Bool(value),
            ConstLit::Nil => Const::Nil,
            ConstLit::Sym(symbol) => Const::Sym(self.str(symbol.text)),
        }
    }

    /// A target, where a name the surface reads cannot reach into a body, nor name
    /// a type it leaves out. Any other declaration left out is not a type, as is
    /// diagnosed, so it is erroneous.
    fn target(&mut self, target: elab::Target) -> WireTarget {
        match target {
            elab::Target::Local(Referent::Decl(decl)) => match self.decl_id(decl) {
                Some(id) => WireTarget::Decl(id),
                None => WireTarget::Error,
            },
            elab::Target::Local(Referent::Binder(found)) => match self.decl_id(found.decl) {
                Some(_) => WireTarget::Binder(self.binder(found)),
                None => WireTarget::Error,
            },
            elab::Target::Local(Referent::Value(value)) => {
                debug_assert_eq!(value.unit, self.base.unit, "a shard refers to its own unit");
                WireTarget::Value(self.span(value.span))
            }
            elab::Target::Local(Referent::Error) => WireTarget::Error,
            elab::Target::Local(Referent::Module(_)) => {
                unreachable!("a link names a module only once resolved")
            }
            elab::Target::Import { module, item } => WireTarget::Import {
                module: index(module.index()),
                item: self.str(item),
            },
            elab::Target::Module(module) => WireTarget::Module(index(module.index())),
        }
    }

    fn arg(&mut self, arg: &surface::TypeArg) -> Arg {
        match &arg.kind {
            TypeArgKind::Pos(ty) => Arg::Pos(self.ty(ty)),
            TypeArgKind::Key { name, ty } => Arg::Key {
                name: self.str(name.text),
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
                        TypeKey::Sym(symbol) => Key::Sym(self.str(symbol.text)),
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
            TypeExpr::Name(head) => Type::Name(self.link(head.link)),
            TypeExpr::Const { value, .. } => {
                Type::Const(value.as_ref().map(|value| self.constant(value)))
            }
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

/// The offset in characters of the start of each line of `content`
fn line_chars(content: &str) -> Vec<u32> {
    let mut starts = vec![0];
    let mut count = 0;
    for ch in content.chars() {
        count += 1;
        if ch == '\n' {
            starts.push(count);
        }
    }
    starts
}

fn quant(quant: TypeQuant) -> Quant {
    match quant {
        TypeQuant::Opt => Quant::Opt,
        TypeQuant::Star => Quant::Star,
        TypeQuant::Include => Quant::Include,
    }
}

/// Decode content into a shard that is not yet linked, and the facts it records.
/// IDs are checked only where decoding follows them; the shard is validated
/// afterwards.
pub(super) fn decode(content: Content<&str>) -> Result<(Shard<'_>, elab::Facts), &'static str> {
    let Content {
        module,
        path,
        lines,
        strict,
        strings,
        types,
        decls,
        facts,
        sites,
        links,
        exports,
        values,
        modules,
        pipes,
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
    let mut shard_decls = Vec::with_capacity(decls.len());
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
        shard_decls.push(elab::Decl {
            unit: local(),
            kind,
            name: Some(own),
            node,
            outer: decl.outer.map(unsig),
            ast: None,
        });
    }
    for (index, fields) in derived {
        let decls =
            (shard_decls.iter().enumerate()).map(|(id, decl)| (DeclId::from_index(id), decl));
        let members = members(decls, DeclId::from_index(index), fields);
        if let DeclNode::Class(class) = &mut shard_decls[index].node {
            class.members = members;
        }
    }

    let facts = unfacts(&shard_decls, facts)?;

    let values: Vec<_> = values
        .into_iter()
        .map(|value| elab::Value {
            span: unspan(value.span),
            annot: unopt(value.annot).map(SiteId::from_index),
        })
        .collect();
    let placed = placed(&shard_decls, &values, sites.len());
    let mut shard_sites = Vec::with_capacity(sites.len());
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
            Ambient::Group => ambient(|decl| shard_decls.get(decl.index()), &decoded),
            Ambient::Sig(found) => Some(unsig(found)),
        };
        shard_sites.push(decoded);
    }

    let shard = Shard {
        info: UnitInfo {
            module: Some(module),
            path: Path::new(path),
            lines,
            source: None,
            strict,
        },
        strings: strings.into_iter().map(Cow::Borrowed).collect(),
        decls: shard_decls,
        sites: shard_sites,
        links: links
            .into_iter()
            .map(|link| elab::Link {
                unit: local(),
                base: untarget(link.base),
                item: unopt(link.item).map(StrId::from_index),
                span: Span::INVALID,
            })
            .collect(),
        referents: Vec::new(),
        refs: FxHashMap::default(),
        modules: modules
            .into_iter()
            .map(|module| elab::Module {
                name: StrId::from_index(module.name as usize),
                span: unspan(module.span),
            })
            .collect(),
        providers: Vec::new(),
        exports: exports
            .into_iter()
            .map(|export| elab::Export {
                name: StrId::from_index(export.name as usize),
                span: unspan(export.span),
                target: untarget(export.target),
            })
            .collect(),
        values,
        sig_decls: FxHashMap::default(),
        sig_owners: Vec::new(),
        kept: None,
        nominations: pipes.map(|pipes| {
            pipes.map(|(module, item)| {
                let str = |id: u32| StrId::from_index(id as usize);
                (str(module), str(item))
            })
        }),
    };
    Ok((shard, facts))
}

/// The facts recorded of decoded declarations: a variance for each binder of each
/// signature, and for outer binders only those of signatures they are nested in
fn unfacts(decls: &[elab::Decl<'_>], facts: Vec<DeclFacts>) -> Result<elab::Facts, &'static str> {
    if facts.len() != decls.len() {
        return Err("the facts are not one per declaration");
    }
    let mut decoded = elab::Facts::default();
    for (index, (decl, facts)) in decls.iter().zip(facts).enumerate() {
        let id = DeclId::from_index(index);
        if facts.variance.len() != decl.node.sig_count() {
            return Err("the variances are not one list per signature");
        }
        for (sig, variances) in facts.variance.into_iter().enumerate() {
            if Some(variances.len()) != slots(&decl.node, sig) {
                return Err("the variances are not one per binder");
            }
            for (slot, found) in variances.into_iter().enumerate() {
                let binder = BinderRef {
                    decl: id,
                    sig,
                    slot,
                };
                decoded.variance.insert(binder, unvariance(found));
            }
        }
        for (binder, found) in facts.captured {
            let binder = unbinder(binder);
            // An outer declaration is before the declarations nested in it
            let (mut outer, mut before) = (decl.outer, index);
            let enclosing = loop {
                match outer {
                    Some((at, sig)) if at.index() < before => {
                        let node = &decls[at.index()].node;
                        if (at, sig) == (binder.decl, binder.sig) {
                            break slots(node, sig).is_some_and(|count| binder.slot < count);
                        }
                        (outer, before) = (decls[at.index()].outer, at.index());
                    }
                    _ => break false,
                }
            };
            if !enclosing {
                return Err("a captured binder is not of an outer signature");
            }
            decoded.captured.insert((id, binder), unvariance(found));
        }
    }
    Ok(decoded)
}

/// Builds a shard's surface from its serialization form
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
