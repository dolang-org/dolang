//! Elaboration of the checked units' declarations into the type database.
//!
//! Collection copies each unit's declaration surface out of its syntax tree (see
//! [`surface`]), and the passes after it read only that and the common tables. Only
//! lowering reads syntax trees again, for the bodies it lowers.

mod alias;
mod capture;
mod collect;
mod ids;
mod interface;
mod judge;
mod kind;
mod overrides;
mod populate;
mod sig;
mod specialize;
pub(crate) mod surface;
mod variance;
mod wellformed;

use std::{
    borrow::Cow,
    fmt::{self, Write},
    path::Path,
};

use bitvec::boxed::BitBox;
use rustc_hash::FxHashMap;

use super::report::{Annotation, Report};
use super::r#type::{
    Database, DeclId, DeclKind, Intrinsic, Kind, Scope, TypeId, UnitId, UnitSpan, Variance,
};
use crate::{
    RestKind, Unit, ast,
    diag::{AnnotationKind, NoteKind, Severity},
    source::{Coord, Span},
};
use surface::{
    Alias, Binder, Class, Closure, Def, Interned, LinkId, Method, ModuleId, Name, Signature,
    SiteId, StrId, TypeExpr,
};

pub(crate) use super::report::{Diag, UnitDiag};
pub(crate) use alias::aliases;
pub(crate) use capture::captures;
pub(crate) use collect::{Export, Link, Module, Value, collect, find_export, link};
pub(crate) use ids::Ids;
pub(crate) use judge::Scoped;
pub(crate) use kind::{Fill, kinds};
pub(crate) use overrides::overrides;
pub(crate) use populate::populate;
pub(crate) use sig::signatures;
pub(crate) use specialize::specialize;
pub(crate) use variance::variances;
pub(crate) use wellformed::{Unresolved, recursion, wellformed};

/// The names of `strand`'s pipe placeholders, sender first
pub(crate) const PIPES: [&str; 2] = ["PipeSender", "PipeReceiver"];

/// What collection learns of the checked units, and what elaboration concludes
/// about their declarations.
///
/// Each unit has a [`Shard`] of its declarations and names, and the [`Facts`]
/// elaboration concludes about its declarations. A unit's declarations, sites and
/// links are each numbered in one contiguous range, so an ID's unit is found by a
/// binary search over the ranges' bases. Only what crosses units is global.
pub(crate) struct Tables<'u> {
    /// What each unit was linked into, by [`UnitId`]
    pub(crate) shards: Vec<Shard<'u>>,
    /// What elaboration concludes about each unit's declarations, by [`UnitId`]
    pub(crate) facts: Vec<Facts>,
    /// The start of each unit's ranges, in the order the units were linked, which
    /// is the order of their IDs
    bases: Vec<Bases>,
    /// The declarations of `std` and `strand` the checker treats specially
    pub(crate) designated: FxHashMap<DeclId, Designated>,
    /// What each of `strand`'s pipe placeholders nominates, by name, resolved where
    /// the placeholder is exported
    pub(crate) nominees: FxHashMap<&'static str, Referent>,
    /// The type each designated pipe placeholder stands for, or `None` for a
    /// nominee that isn't checked or can't stand for it
    pub(crate) pipes: FxHashMap<DeclId, Option<DeclId>>,
}

/// What the checker knows of a unit's declarations and the names written in them.
///
/// Collection fills a shard from a unit's source, and a typelib decodes to one. It
/// is local until it is linked: its unit is unit 0, and its declarations, sites and
/// links are numbered from 0. Linking moves it to its unit and ranges, and fills
/// what only linking can tell: `referents`, `providers`, `sig_decls`,
/// `sig_owners` and `kept`, which are empty until then.
#[derive(Clone)]
pub(crate) struct Shard<'u> {
    pub(crate) info: UnitInfo<'u>,
    /// The unit's text, by [`StrId`]: each string once. Most borrow the unit's
    /// source or typelib; a constant type's string is folded from it.
    pub(crate) strings: Vec<Cow<'u, str>>,
    /// The unit's declarations, from its base [`DeclId`]
    pub(crate) decls: Vec<Decl<'u>>,
    /// Every type expression written in a declaration or annotation of the unit,
    /// outermost only, from its base [`SiteId`]
    pub(crate) sites: Vec<Site>,
    /// What the unit's names refer to as far as the unit can tell, each once, from
    /// its base [`LinkId`]
    pub(crate) links: Vec<Link>,
    /// What each of the unit's links refers to, from its base [`LinkId`]. Imports
    /// and renames are chased away; aliases are not, so `Pair[Int]` refers to the
    /// `Pair` alias.
    pub(crate) referents: Vec<Referent>,
    /// The link of each name a unit with source writes, by the span of its head.
    /// Only lowering and the analysis, which read syntax, look a name up.
    pub(crate) refs: FxHashMap<Span, LinkId>,
    /// The modules the unit imports, and the prelude modules it uses, by
    /// [`ModuleId`]
    pub(crate) modules: Vec<Module>,
    /// The unit providing each module of the unit's module table, by [`ModuleId`],
    /// or `None` for a module no unit provides
    pub(crate) providers: Vec<Option<UnitId>>,
    /// The unit's exports, sorted by name. Empty for a unit that is not a module.
    pub(crate) exports: Vec<Export>,
    /// Each exported variable, in the order of the spans of their names
    pub(crate) values: Vec<Value>,
    /// The database declaration of each def or method signature. A function's own
    /// ID holds its implementation, or its first signature when it has none.
    pub(crate) sig_decls: FxHashMap<(DeclId, usize), DeclId>,
    /// The signature each of the unit's other database declarations is of. They
    /// follow its declarations.
    pub(crate) sig_owners: Vec<(DeclId, usize)>,
    /// Whether a typelib of the unit keeps each of its declarations, by offset, for
    /// a unit with source whose interface names no private declaration
    pub(crate) kept: Option<BitBox>,
    /// The types `strand`'s pipe placeholders stood for, each as its module and
    /// item, when the facts a typelib records were concluded, if they depend on
    /// them. `None` for a unit with source.
    pub(crate) nominations: Option<[(StrId, StrId); 2]>,
}

impl Shard<'_> {
    /// An exported variable, by the span of its name
    pub(crate) fn value(&self, span: Span) -> Option<&Value> {
        self.values
            .binary_search_by_key(&(span.start, span.end), |value| {
                (value.span.start, value.span.end)
            })
            .ok()
            .map(|index| &self.values[index])
    }
}

/// What elaboration concludes about one unit's declarations. A unit's facts start
/// empty, or with what its typelib records.
#[derive(Clone, Default)]
pub(crate) struct Facts {
    /// The underlying head of each transparent alias
    pub(crate) aliases: FxHashMap<DeclId, Head>,
    /// The kind of each binder, including the implicit binders of omitted ambient
    /// channels
    pub(crate) binder_kinds: FxHashMap<BinderRef, KindOf>,
    /// The kind of each alias
    pub(crate) alias_kinds: FxHashMap<DeclId, KindOf>,
    /// The completed signature of each def or method, by declaration and signature
    pub(crate) sigs: FxHashMap<(DeclId, usize), Sig>,
    /// The type of each field, by its class and the span of its name
    pub(crate) fields: FxHashMap<(DeclId, Span), Slot>,
    /// The variance of each binder, including the implicit binders of omitted ambient
    /// channels
    pub(crate) variance: FxHashMap<BinderRef, Variance>,
    /// The variance of each binder of an enclosing declaration that a nested one uses.
    /// A binder it does not use is absent, and invariant if it is captured anyway.
    pub(crate) captured: FxHashMap<(DeclId, BinderRef), Variance>,
    /// The binders of enclosing declarations each declaration is lifted over, which
    /// lead its binder group, outermost first
    pub(crate) lifted: FxHashMap<DeclId, Vec<BinderRef>>,
    /// The binder group of each declaration signature: the binders it is lifted
    /// over, its written binders, then its implicit binders
    pub(crate) groups: FxHashMap<(DeclId, usize), Vec<BinderRef>>,
}

/// Where a unit's ranges of IDs start
#[derive(Clone, Copy)]
pub(crate) struct Bases {
    pub(crate) unit: UnitId,
    /// Its declarations, then the database declarations of its signatures
    pub(crate) decls: usize,
    pub(crate) sites: usize,
    pub(crate) links: usize,
}

impl Bases {
    /// The bases of a shard that is not yet linked
    pub(crate) fn local() -> Self {
        Self {
            unit: local(),
            decls: 0,
            sites: 0,
            links: 0,
        }
    }
}

impl<'u> Tables<'u> {
    /// The tables of linked shards and their facts, whose bases are in the order
    /// they were linked
    pub(crate) fn new(shards: Vec<Shard<'u>>, facts: Vec<Facts>, bases: Vec<Bases>) -> Self {
        Self {
            shards,
            facts,
            bases,
            designated: FxHashMap::default(),
            nominees: FxHashMap::default(),
            pipes: FxHashMap::default(),
        }
    }

    /// The bases of the unit whose range holds `index`, by `base`
    fn bases(&self, index: usize, base: impl Fn(&Bases) -> usize) -> &Bases {
        let at = self.bases.partition_point(|bases| base(bases) <= index);
        &self.bases[at.checked_sub(1).expect("every ID is in a unit's range")]
    }

    /// The unit a declaration, or a signature's database declaration, is of
    pub(crate) fn unit_of(&self, id: DeclId) -> UnitId {
        self.bases(id.index(), |bases| bases.decls).unit
    }

    /// The shard of a unit
    pub(crate) fn shard(&self, unit: UnitId) -> &Shard<'u> {
        &self.shards[unit.index()]
    }

    /// What the checker knows of a unit
    pub(crate) fn info(&self, unit: UnitId) -> &UnitInfo<'u> {
        &self.shards[unit.index()].info
    }

    /// A declaration
    pub(crate) fn decl(&self, id: DeclId) -> &Decl<'u> {
        let bases = self.bases(id.index(), |bases| bases.decls);
        &self.shards[bases.unit.index()].decls[id.index() - bases.decls]
    }

    /// A declaration, or `None` for a signature's database declaration
    pub(crate) fn try_decl(&self, id: DeclId) -> Option<&Decl<'u>> {
        let bases = self.bases(id.index(), |bases| bases.decls);
        self.shards[bases.unit.index()]
            .decls
            .get(id.index() - bases.decls)
    }

    /// The def or method signature whose database declaration `id` is, unless it
    /// is a declaration's own
    pub(crate) fn sig_owner(&self, id: DeclId) -> Option<(DeclId, usize)> {
        let bases = self.bases(id.index(), |bases| bases.decls);
        let shard = &self.shards[bases.unit.index()];
        let offset = id.index() - bases.decls;
        offset
            .checked_sub(shard.decls.len())
            .map(|offset| shard.sig_owners[offset])
    }

    /// Every declaration, in order
    pub(crate) fn decl_ids(&self) -> impl Iterator<Item = DeclId> + '_ {
        self.bases.iter().flat_map(|bases| {
            let count = self.shards[bases.unit.index()].decls.len();
            (bases.decls..bases.decls + count).map(DeclId::from_index)
        })
    }

    /// The number of declaration IDs, which number every unit's declarations and
    /// the database declarations of their signatures
    pub(crate) fn decl_id_count(&self) -> usize {
        self.bases.last().map_or(0, |bases| {
            let shard = &self.shards[bases.unit.index()];
            bases.decls + shard.decls.len() + shard.sig_owners.len()
        })
    }

    /// The IDs of a unit's declarations, in order
    pub(crate) fn unit_decl_ids(&self, unit: UnitId) -> impl Iterator<Item = DeclId> + use<> {
        let base = self.base(unit).decls;
        (base..base + self.shards[unit.index()].decls.len()).map(DeclId::from_index)
    }

    /// A site
    pub(crate) fn site(&self, id: SiteId) -> &Site {
        let bases = self.bases(id.index(), |bases| bases.sites);
        &self.shards[bases.unit.index()].sites[id.index() - bases.sites]
    }

    /// Every site, in order
    pub(crate) fn sites(&self) -> impl Iterator<Item = (SiteId, &Site)> + '_ {
        self.bases.iter().flat_map(|bases| {
            let sites = &self.shards[bases.unit.index()].sites;
            sites
                .iter()
                .enumerate()
                .map(|(index, site)| (SiteId::from_index(bases.sites + index), site))
        })
    }

    /// A unit's sites, in order
    pub(crate) fn unit_sites(&self, unit: UnitId) -> impl Iterator<Item = (SiteId, &Site)> + '_ {
        let base = self.base(unit).sites;
        self.shards[unit.index()]
            .sites
            .iter()
            .enumerate()
            .map(move |(index, site)| (SiteId::from_index(base + index), site))
    }

    /// The bases of a unit
    pub(crate) fn base(&self, unit: UnitId) -> &Bases {
        self.bases
            .iter()
            .find(|bases| bases.unit == unit)
            .expect("every unit is linked")
    }

    /// What a unit's declarations are concluded to be
    pub(crate) fn facts_of(&self, id: DeclId) -> &Facts {
        &self.facts[self.unit_of(id).index()]
    }

    /// What a unit's declarations are concluded to be, to add to
    pub(crate) fn facts_mut(&mut self, id: DeclId) -> &mut Facts {
        let unit = self.unit_of(id);
        &mut self.facts[unit.index()]
    }

    /// The underlying head of a transparent alias
    pub(crate) fn alias(&self, decl: DeclId) -> Option<&Head> {
        self.facts_of(decl).aliases.get(&decl)
    }

    /// The kind of a binder
    pub(crate) fn binder_kind(&self, binder: BinderRef) -> Option<KindOf> {
        self.facts_of(binder.decl)
            .binder_kinds
            .get(&binder)
            .copied()
    }

    /// The kind of an alias
    pub(crate) fn alias_kind(&self, decl: DeclId) -> Option<KindOf> {
        self.facts_of(decl).alias_kinds.get(&decl).copied()
    }

    /// The completed signature `sig` of a def or method
    pub(crate) fn sig(&self, decl: DeclId, sig: usize) -> Option<&Sig> {
        self.facts_of(decl).sigs.get(&(decl, sig))
    }

    /// The type of a class's field, by the span of its name
    pub(crate) fn field(&self, class: DeclId, name: Span) -> Option<Slot> {
        self.facts_of(class).fields.get(&(class, name)).copied()
    }

    /// The variance of a binder
    pub(crate) fn variance(&self, binder: BinderRef) -> Option<Variance> {
        self.facts_of(binder.decl).variance.get(&binder).copied()
    }

    /// The binders of enclosing declarations a declaration is lifted over
    pub(crate) fn lifted(&self, decl: DeclId) -> Option<&[BinderRef]> {
        self.facts_of(decl).lifted.get(&decl).map(Vec::as_slice)
    }

    /// The binder group of a declaration signature
    pub(crate) fn group(&self, key: (DeclId, usize)) -> Option<&[BinderRef]> {
        self.facts_of(key.0).groups.get(&key).map(Vec::as_slice)
    }

    /// The database declaration of a def or method signature
    pub(crate) fn sig_decl(&self, key: (DeclId, usize)) -> Option<DeclId> {
        self.shard(self.unit_of(key.0)).sig_decls.get(&key).copied()
    }

    /// What a name refers to, given its link
    pub(crate) fn linked(&self, link: Option<LinkId>) -> Option<&Referent> {
        link.map(|link| {
            let bases = self.bases(link.index(), |bases| bases.links);
            &self.shards[bases.unit.index()].referents[link.index() - bases.links]
        })
    }

    /// What a name written in a unit with source refers to, by the span of its head
    pub(crate) fn written(&self, unit: UnitId, head: Span) -> Option<&Referent> {
        self.linked(self.shards[unit.index()].refs.get(&head).copied())
    }

    /// The number of signatures of a declaration: its defs or methods, or 1
    pub(crate) fn sig_count(&self, decl: DeclId) -> usize {
        self.decl(decl).node.sig_count()
    }

    /// The binders written for signature `sig` of a declaration
    pub(crate) fn binders(&self, decl: DeclId, sig: usize) -> &[Binder] {
        match &self.decl(decl).node {
            DeclNode::Class(class) => &class.binders,
            DeclNode::Alias(alias) => &alias.binders,
            DeclNode::Defs(defs) => &defs[sig].binders,
            DeclNode::Methods(methods) => &methods[sig].binders,
            DeclNode::Closure(_) => &[],
        }
    }

    /// The declaration designated `role`, if it is checked
    pub(crate) fn designated_decl(&self, role: Designated) -> Option<DeclId> {
        self.designated
            .iter()
            .find(|&(_, &designated)| designated == role)
            .map(|(&decl, _)| decl)
    }

    /// Signature `sig` of a method declaration
    pub(crate) fn method(&self, decl: DeclId, sig: usize) -> &Method {
        match &self.decl(decl).node {
            DeclNode::Methods(methods) => &methods[sig],
            _ => unreachable!("only a method declaration has methods"),
        }
    }

    /// Each written type as interned, with the kinds of the binders of the group it
    /// is interpreted in
    pub(crate) fn site_kinds(&self) -> impl Iterator<Item = (TypeId, Vec<Kind>)> + '_ {
        self.sites().map(|(_, site)| {
            let ty = site.interned.get().expect("population interns each site");
            let kinds = site.group().map_or_else(Vec::new, |key| {
                self.group(key)
                    .expect("population groups each signature")
                    .iter()
                    .map(|&binder| {
                        self.binder_kind(binder)
                            .expect("every binder has a kind")
                            .kind
                    })
                    .collect()
            });
            (ty, kinds)
        })
    }

    /// The rigids a declaration signature's body is checked under, one for each
    /// binder of its group. A binder lifted from an enclosing declaration is that
    /// declaration's rigid, so a closure's types agree with its enclosing def's.
    pub(crate) fn group_rigids(&self, db: &Database, key: (DeclId, usize)) -> Vec<TypeId> {
        let Some(group) = self.group(key) else {
            return Vec::new();
        };
        group
            .iter()
            .map(|binder| {
                let owner = (binder.decl, binder.sig);
                let slot = self
                    .group(owner)
                    .expect("a binder's declaration is grouped")
                    .iter()
                    .position(|other| other == binder)
                    .expect("a binder is in its own declaration's group");
                let rigids = db.rigids(self.sig_decl(owner).expect("every signature is declared"));
                // A group too large to populate left its declaration without binders
                rigids.get(slot).copied().unwrap_or_else(|| {
                    db.unknown_of(
                        self.binder_kind(*binder)
                            .expect("every binder has a kind")
                            .kind,
                    )
                })
            })
            .collect()
    }

    /// The signature whose database declaration a declaration's own ID holds: its
    /// implementation, or its first signature when it has none
    pub(crate) fn primary_sig(&self, decl: DeclId) -> usize {
        self.decl(decl).node.primary_sig().unwrap_or(0)
    }

    /// The export of `unit` named `name`
    pub(crate) fn export(&self, unit: UnitId, name: &str) -> Option<&Export> {
        let shard = &self.shards[unit.index()];
        find_export(&shard.exports, &shard.strings, name)
    }

    /// The source text of a span of a unit, for lowering and flow analysis, which
    /// read only units with source
    pub(crate) fn text(&self, unit: UnitId, span: Span) -> &'u str {
        self.shards[unit.index()]
            .info
            .source
            .expect("only a unit with source is read as text")
            .file
            .str(span)
    }
}

/// The unit of a shard that is not yet linked
pub(crate) fn local() -> UnitId {
    UnitId::from_index(0)
}

/// What the checker knows of a unit, whether or not its source is at hand
#[derive(Clone)]
pub(crate) struct UnitInfo<'u> {
    /// The module's name; absent for a script
    pub(crate) module: Option<&'u str>,
    pub(crate) path: &'u Path,
    /// Where the lines of a unit without source start, which locates the spans it
    /// keeps: the number and start of each line holding one, in order. The spans
    /// of a unit without source, and these starts, are offsets in characters, not
    /// bytes. A unit with source is located by its file.
    pub(crate) lines: Vec<(u32, u32)>,
    /// The unit, when it is checked from source
    pub(crate) source: Option<&'u Unit<'u>>,
    /// Whether the unit is checked strictly
    pub(crate) strict: bool,
}

impl UnitInfo<'_> {
    /// The line and column of an offset of the unit, in bytes for a unit with
    /// source and in characters for one without
    pub(crate) fn coord(&self, offset: u32) -> Coord {
        if let Some(source) = self.source {
            return source.file.coord(offset);
        }
        let index = self.lines.partition_point(|&(_, start)| start <= offset);
        match index.checked_sub(1).map(|index| self.lines[index]) {
            Some((line, start)) => Coord {
                line,
                column: offset - start,
            },
            None => Coord {
                line: 0,
                column: offset,
            },
        }
    }

    /// A module's name, or a script's file stem
    pub(crate) fn name(&self) -> String {
        match self.module {
            Some(name) => name.to_owned(),
            None => self
                .path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned()),
        }
    }
}

/// A binder: slot `slot` of signature `sig` of a declaration. `sig` indexes the
/// declaration's defs or methods, and is 0 for any other declaration. The implicit
/// binders of a signature's omitted ambient channels follow its written binders.
/// Outer declarations are allocated first, so the order is outermost first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BinderRef {
    pub(crate) decl: DeclId,
    pub(crate) sig: usize,
    pub(crate) slot: usize,
}

/// A type expression written in source, and how it is used
#[derive(Clone)]
pub(crate) struct Site {
    /// The shard's own unit, which a typelib leaves implicit
    pub(crate) unit: UnitId,
    pub(crate) ty: TypeExpr,
    pub(crate) role: Role,
    /// The def or method signature whose ambient channels a function type written
    /// here takes when it omits its own
    pub(crate) ambient: Option<(DeclId, usize)>,
    /// The declaration signature whose body or signature the type is written in,
    /// absent at a unit's top level
    pub(crate) owner: Option<(DeclId, usize)>,
    /// The type as interned in its group
    pub(crate) interned: Interned,
}

impl Site {
    /// The declaration signature whose binder group the type is interpreted in
    pub(crate) fn group(&self) -> Option<(DeclId, usize)> {
        match self.role {
            Role::Bound(binder) | Role::Default(binder) => Some((binder.decl, binder.sig)),
            Role::Alias(decl) => Some((decl, 0)),
            Role::Type | Role::Rest | Role::Pattern => self.owner,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// The annotation of a binding, or a return type or ambient channel: a type
    Type,
    /// The annotation of a rest binding: a type for each item, or a schema for the
    /// whole pack
    Rest,
    /// A pattern a rest binding's `@...` expands over the packs it names
    Pattern,
    /// A binder's bound
    Bound(BinderRef),
    /// A binder's default
    Default(BinderRef),
    /// The body of a transparent alias
    Alias(DeclId),
}

/// A kind, and whether nothing determined it but a name with no known kind
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KindOf {
    pub(crate) kind: Kind,
    /// No declaration determined the kind, and an external or erroneous name might
    /// have, so a mismatch is not reported
    pub(crate) flexible: bool,
}

/// A type in a completed signature, before it is interned
#[derive(Clone, Copy)]
pub(crate) enum Slot {
    Annot(SiteId),
    /// Omitted, so dynamic
    Unknown,
    /// An omitted receiver annotation: the class applied to its own binders
    SelfType,
    /// An omitted class method receiver annotation: `Type` of the class applied to
    /// its own binders
    ObjectType,
    /// A strict unit's omitted return type
    Nil,
}

/// The type of a rest parameter, before it is interned
#[derive(Clone, Copy)]
pub(crate) enum RestSlot {
    /// A type for each item: `{*T}`, `{**T}` or `{*T, **T}` by the rest's kind
    Items(RestKind, Slot),
    /// A schema for the whole pack
    Pack(SiteId),
    /// A type pattern expanded over the packs it names
    Pattern(SiteId),
}

#[derive(Clone, Copy)]
pub(crate) enum ParamTy {
    Single(Slot),
    Rest(RestSlot),
}

/// An ambient channel of a signature or function type
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ambient {
    /// Written where it is used
    Written,
    /// An implicit binder of a signature that omits the channel
    Implicit(BinderRef),
    /// The channel written on the def or method signature `(decl, sig)`
    Of(DeclId, usize),
    /// `Value`, outside a def's signature
    Value,
}

/// A def or method signature, completed with the defaults for what it omits
#[derive(Clone)]
pub(crate) struct Sig {
    /// Each parameter's type, by its index in the signature's parameters
    pub(crate) params: Vec<ParamTy>,
    /// The scope of the method whose receiver the first parameter is, if any:
    /// an instance or class method's
    pub(crate) receiver: Option<Scope>,
    pub(crate) input: Ambient,
    pub(crate) output: Ambient,
    pub(crate) ret: Slot,
}

/// A declaration of `std` or `strand` the checker treats specially
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Designated {
    /// `std.Value`, which is top
    Value,
    /// `std.Never`, which is bottom
    Never,
    /// `std.Unknown`, the dynamic type
    Unknown,
    /// `std.Phantom`, which marks its arguments as used covariantly
    Phantom,
    /// `std.getter`, which makes a method a computed field's getter
    Getter,
    /// `std.setter`, which makes a method a computed field's setter
    Setter,
    /// `std.final`, which forbids subclassing a class
    Final,
    /// `std.mixable`, which lets a class be combined with unrelated classes
    Mixable,
    /// `std.Fmt`, the value of a `t"..."` sequence
    Fmt,
    /// `std.FmtValue`, an interpolation binding a value to a specification
    FmtValue,
    /// `std.FmtParam`, an unbound `${#...}` interpolation
    FmtParam,
    /// `std.Float`, the class of a float literal
    Float,
    /// `std.Bin`, the class of a binary string
    Bin,
    /// `std.Array`, the class of an array literal
    Array,
    /// `std.Dict`, the class of a dict literal
    Dict,
    /// `std.Record`, the class of a record literal
    Record,
    /// `std.Range`, the class of a range
    Range,
    /// `std.Spread`, which a spread item spreads
    Spread,
    /// `std.Unpack`, which a pattern unpacks
    Unpack,
    /// `strand.PipeSender`, which stands for the embedding's pipe sender
    PipeSender,
    /// `strand.PipeReceiver`, which stands for the embedding's pipe receiver
    PipeReceiver,
    Intrinsic(Intrinsic),
}

/// A source declaration
#[derive(Clone)]
pub(crate) struct Decl<'u> {
    /// The shard's own unit, which a typelib leaves implicit
    pub(crate) unit: UnitId,
    pub(crate) kind: DeclKind,
    /// The declared name; absent for a closure
    pub(crate) name: Option<Name>,
    pub(crate) node: DeclNode,
    /// The declaration this one is nested in, and which of its signatures, whose
    /// binders it may capture
    pub(crate) outer: Option<(DeclId, usize)>,
    /// The syntax the declaration was collected from, for lowering; absent for a
    /// unit checked without its source
    pub(crate) ast: Option<DeclAst<'u>>,
}

/// A declaration's surface
#[derive(Clone)]
pub(crate) enum DeclNode {
    /// A class or protocol
    Class(Class),
    /// A transparent or opaque alias
    Alias(Alias),
    /// A function's implementation and its `@def` overloads, in source order. The
    /// implementation is absent when only overloads were written.
    Defs(Vec<Def>),
    /// The methods of one name in a class body, in source order
    Methods(Vec<Method>),
    /// A lambda or field initializer
    Closure(Closure),
}

/// The syntax a declaration was collected from
#[derive(Clone)]
pub(crate) enum DeclAst<'u> {
    Class(&'u ast::Class),
    Alias(#[expect(dead_code, reason = "an alias has no body to lower")] &'u ast::TypeAlias),
    Defs(Vec<&'u ast::Def>),
    Methods(Vec<&'u ast::Method>),
    Closure(&'u ast::Function),
}

impl Decl<'_> {
    /// The span of the declared name; a declaration that is a type is named
    pub(crate) fn name_span(&self) -> Option<Span> {
        self.name.map(|name| name.span)
    }
}

impl DeclNode {
    /// The number of signatures: its defs or methods, or 1
    pub(crate) fn sig_count(&self) -> usize {
        match self {
            DeclNode::Defs(defs) => defs.len(),
            DeclNode::Methods(methods) => methods.len(),
            DeclNode::Class(_) | DeclNode::Alias(_) | DeclNode::Closure(_) => 1,
        }
    }

    /// The signature whose database declaration the declaration's own ID holds: a
    /// def's or method's implementation, if it has one
    pub(crate) fn primary_sig(&self) -> Option<usize> {
        match self {
            DeclNode::Defs(defs) => defs.iter().position(|def| !def.type_only),
            // A protocol member is type-only, but not an overload signature
            DeclNode::Methods(methods) => methods.iter().position(|method| !method.overload),
            DeclNode::Class(_) | DeclNode::Alias(_) | DeclNode::Closure(_) => None,
        }
    }

    /// The signature of a def, method or closure
    pub(crate) fn signature(&self, sig: usize) -> &Signature {
        match self {
            DeclNode::Defs(defs) => &defs[sig].sig,
            DeclNode::Methods(methods) => &methods[sig].sig,
            DeclNode::Closure(closure) => &closure.sig,
            DeclNode::Class(_) | DeclNode::Alias(_) => {
                unreachable!("only a def, method or closure has a signature")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Referent {
    Decl(DeclId),
    Binder(BinderRef),
    /// A binding that exists only at runtime, such as a `let` or a parameter
    Value(UnitSpan),
    /// Nothing, for a reason already diagnosed
    Error,
    /// A checked module, reached by a name that is not dotted, which a shard
    /// names only once linked
    Module(UnitId),
}

/// What an unresolved name or export refers to. A module is one of its unit's
/// module table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    Local(Referent),
    /// An item of a module, by name
    Import {
        module: ModuleId,
        item: StrId,
    },
    Module(ModuleId),
}

/// The head of a transparent alias's underlying type
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Head {
    /// A class, protocol or opaque alias, or a declaration that is not a type
    Decl(DeclId),
    Binder(BinderRef),
    /// A union, function type, schema or constant
    Structural,
    /// Nothing, for a reason already diagnosed
    Error,
}

struct ImportCycle {
    span: Span,
    chain: String,
}

impl Report for ImportCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "import cycle")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        vec![(
            NoteKind::Info,
            format!("{} re-export each other", self.chain),
        )]
    }
}

struct AliasCycle(Span);

impl Report for AliasCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "alias refers to itself")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct MissingModule {
    span: Span,
    module: String,
}

impl Report for MissingModule {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "module `{}` not found", self.module)
    }

    fn span(&self) -> Span {
        self.span
    }
}

struct MissingExport {
    span: Span,
    module: String,
    item: String,
}

impl Report for MissingExport {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "module `{}` has no export `{}`", self.module, self.item)
    }

    fn span(&self) -> Span {
        self.span
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Type => "a type",
        Kind::Schema => "a schema",
    }
}

/// A type name that names something other than a type
struct NotAType {
    span: Span,
    name: String,
}

impl Report for NotAType {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`{}` is not a type", self.name)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A type or schema where the other is expected
#[derive(Clone)]
struct KindMismatch {
    span: Span,
    expected: Kind,
    /// Where the kind was declared, when in the same unit
    declared: Option<Span>,
    /// The schema alias named where a type is expected, which a dict could take
    alias: Option<String>,
}

impl Report for KindMismatch {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        let found = match self.expected {
            Kind::Type => Kind::Schema,
            Kind::Schema => Kind::Type,
        };
        write!(
            w,
            "expected {}, found {}",
            kind_name(self.expected),
            kind_name(found)
        )
    }

    fn span(&self) -> Span {
        self.span
    }

    fn annotations(&self) -> Vec<Annotation> {
        declared_here(self.declared)
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        self.alias
            .iter()
            .map(|alias| {
                (
                    NoteKind::Help,
                    format!("for a dict with this schema, write `Dict[{alias}]`"),
                )
            })
            .collect()
    }
}

/// A note of where something was declared, when in the same unit
fn declared_here(declared: Option<Span>) -> Vec<Annotation> {
    declared
        .map(|span| Annotation {
            kind: AnnotationKind::Context,
            span,
            message: "declared here".to_owned(),
        })
        .into_iter()
        .collect()
}

/// Type arguments applied to what takes none
struct NotGeneric {
    span: Span,
    /// Whether the base is a schema rather than a non-generic type
    schema: bool,
}

impl Report for NotGeneric {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        if self.schema {
            write!(w, "a schema takes no type arguments")
        } else {
            write!(w, "type takes no type arguments")
        }
    }

    fn span(&self) -> Span {
        self.span
    }
}

struct TooManyTypeArgs(Span);

impl Report for TooManyTypeArgs {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "too many type arguments")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct UnknownTypeKeyword {
    span: Span,
    name: String,
}

impl Report for UnknownTypeKeyword {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "no binder takes the keyword type argument `{}`",
            self.name
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A receiver annotation that doesn't reach its method's class
struct BadReceiver {
    span: Span,
    class: String,
    /// The walk to the class could not be decided either way
    undecided: bool,
}

impl Report for BadReceiver {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.undecided {
            false => write!(w, "`self` must be a `{}` or a subtype of it", self.class),
            true => write!(
                w,
                "cannot tell whether this is a `{}` or a subtype of it",
                self.class
            ),
        }
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A method implementation's receiver annotation that specializes its class,
/// which only an overload may do
struct SpecializedReceiver {
    span: Span,
    class: String,
}

impl Report for SpecializedReceiver {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "only an `@def` overload may specialize `self` to a `{}`",
            self.class
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A type argument, or a binder's default, that doesn't satisfy its binder's bound
struct BoundViolation {
    span: Span,
    /// The binder with its bound, as written: `T @ Num`
    binder: String,
    default: bool,
}

impl Report for BoundViolation {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.default {
            false => write!(w, "this does not satisfy `{}`", self.binder),
            true => write!(w, "the default does not satisfy `{}`", self.binder),
        }
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A function type or signature whose parameters admit keys that aren't symbols
struct ParameterKeys(Span);

impl Report for ParameterKeys {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "function parameters must have symbol keys")
    }

    fn span(&self) -> Span {
        self.0
    }
}

/// A recursive alias reference that isn't guarded, or doesn't pass its binders
/// unchanged
struct BadRecursion {
    span: Span,
    alias: String,
    /// It is guarded, but not regular
    irregular: bool,
}

impl Report for BadRecursion {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.irregular {
            false => write!(
                w,
                "recursive reference to `{}` must be inside a class's arguments, a function type or a schema",
                self.alias
            ),
            true => write!(
                w,
                "recursive reference to `{}` must pass its binders unchanged",
                self.alias
            ),
        }
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A member that doesn't conform to a supertype's, or a class a supertype names
/// that isn't inherited
struct Nonconforming {
    span: Span,
    message: String,
}

impl Report for Nonconforming {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        w.write_str(&self.message)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A class inheriting from two disjoint classes neither of which reaches the
/// other
struct IncoherentClasses {
    span: Span,
    class: String,
    first: String,
    second: String,
}

impl Report for IncoherentClasses {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "`{}` inherits from unrelated disjoint classes `{}` and `{}`",
            self.class, self.first, self.second
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// An instance `(call)` of a class that doesn't reach `Func`, so its instances
/// aren't passed as functions
struct Uncallable {
    span: Span,
    class: String,
}

impl Report for Uncallable {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "`{}` has a `(call)` but not `Func` as a supertype, so it isn't passed as a function",
            self.class
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// An instance method that a class object reaches before a class or static
/// member of the same name, leaving that member reachable only through
/// `type(C)`
struct Hidden {
    span: Span,
    /// The instance method, qualified by its class
    method: String,
    /// The member it hides, qualified by its class
    member: String,
    scope: Scope,
    /// The class whose object hides it
    class: String,
}

impl Report for Hidden {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        let scope = match self.scope {
            Scope::Static => "static",
            Scope::Class | Scope::Instance => "class",
        };
        write!(
            w,
            "instance method `{}` hides {scope} member `{}`, which is reachable only through `type({})`",
            self.method, self.member, self.class
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A rest binding's `@...` pattern that names no pack
struct PatternWithoutPack(Span);

impl Report for PatternWithoutPack {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`...` pattern names no pack to expand over")
    }

    fn span(&self) -> Span {
        self.0
    }
}

/// A declaration of `std` with a designated name but the wrong kind of declaration
struct MisdeclaredIntrinsic {
    span: Span,
    expected: &'static str,
}

impl Report for MisdeclaredIntrinsic {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "the checker requires this to be {}", self.expected)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A designated declaration the checker requires that `std` doesn't declare. Each
/// check that needs it is left undecided, without its own report.
struct MissingIntrinsic {
    span: Span,
    name: &'static str,
}

impl Report for MissingIntrinsic {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "`std` doesn't declare `{}`, which the checker requires",
            self.name
        )
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// An annotation a strict unit omits
#[derive(Clone)]
struct MissingAnnotation {
    span: Span,
    what: &'static str,
}

impl Report for MissingAnnotation {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "a strict unit must annotate this {}", self.what)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A pipe placeholder's nominee that can't stand for it
#[derive(Clone)]
struct BadNominee {
    span: Span,
    /// The nominee, qualified by its module
    nominee: String,
    placeholder: &'static str,
    /// Where the nominee was declared, when in the same unit
    declared: Option<Span>,
    /// The nominee is not a class or protocol, rather than a class that can't take
    /// the placeholder's type arguments
    not_class: bool,
}

impl Report for BadNominee {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.not_class {
            true => write!(
                w,
                "`{}` must be a class to stand for `{}`",
                self.nominee, self.placeholder
            ),
            false => write!(
                w,
                "`{}` can't take the type arguments of `{}`",
                self.nominee, self.placeholder
            ),
        }
    }

    fn span(&self) -> Span {
        self.span
    }

    fn annotations(&self) -> Vec<Annotation> {
        declared_here(self.declared)
    }
}
