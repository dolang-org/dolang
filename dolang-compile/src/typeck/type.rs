//! Canonical structures and allocated source declarations.
//!
//! IDs belong to one database; they must not be mixed between databases. Equality
//! is structural, not a subtype judgment. In particular, equal open types can
//! mean different things in different environments. Declaration references retain
//! source identity and are leaves of structural traversal.
//!
//! Each nonempty quantifier introduces one group. Its entire group is in scope
//! in its bounds, defaults, and body. A mapping's pattern is in a group of its
//! own, of an item of each pack. A bound reference counts groups outward, then
//! selects a slot in declaration order. Declarations' binder metadata is
//! parallel to the outer structural group, never a second quantifier.
//!
//! A source declaration is closed: its outer group is one flat group of the outer
//! binders it captures (lifted), then its written binders, then its implicit ambient
//! binders. Where each slot came from is metadata only.
//!
//! A rigid stands for a binder of a declaration while it is checked. Rigids are
//! closed and interned like any type, but never appear in a declaration.
//!
//! Solver variables, skolems, and flow state do not belong here. `Unknown` is the
//! dynamic type an omitted `def` annotation stands for, and what an erroneous site
//! is interned as; it is not a marker of either. It has a schema-kinded twin.
//! Consumers interpret free references through their own environments.
//! Invalid construction, lifecycle misuse, and representation overflow panic.
//! Source complexity limits must be enforced before constructing these structures.
//! Exposure returns definitions in their defining environment: a consumer must
//! preserve that environment when instantiating or substituting an open type.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use dolang_util::{alias, intern};

use crate::source::Span;

macro_rules! id {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub(crate) struct $name(NonZeroU32);

        impl $name {
            pub(crate) fn from_index(index: usize) -> Self {
                Self(
                    NonZeroU32::new(
                        u32::try_from(index.checked_add(1).expect("database too large"))
                            .expect("database too large"),
                    )
                    .unwrap(),
                )
            }

            pub(crate) fn index(self) -> usize {
                self.0.get() as usize - 1
            }
        }
    };
}

id!(DeclId);
pub(crate) use crate::UnitId;

pub(crate) struct TypeTag;
pub(crate) type TypeId = intern::Id<TypeTag>;

pub(crate) struct SymbolTag;
pub(crate) type SymbolId = intern::Id<SymbolTag>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Type,
    Schema,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BoundRef {
    pub(crate) depth: u16,
    pub(crate) slot: u16,
}

impl BoundRef {
    pub(crate) fn new(depth: usize, slot: usize) -> Self {
        Self {
            depth: depth.try_into().expect("binder depth overflow"),
            slot: slot.try_into().expect("binder slot overflow"),
        }
    }
}

/// Exact values, not interpreter builtin types. `Int`, `Sym`, etc. are declarations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Literal {
    Nil,
    Bool(bool),
    Int(i128),
    Str(Box<str>),
    Sym(SymbolId),
}

impl Literal {
    /// The class of the literal's value
    pub(crate) fn intrinsic(&self) -> Intrinsic {
        match self {
            Self::Nil => Intrinsic::Nil,
            Self::Bool(_) => Intrinsic::Bool,
            Self::Int(_) => Intrinsic::Int,
            Self::Str(_) => Intrinsic::Str,
            Self::Sym(_) => Intrinsic::Sym,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Variance {
    Invariant,
    Covariant,
    Contravariant,
}

impl Variance {
    /// The variance of a position `inner` to one that is itself `self`
    pub(crate) fn compose(self, inner: Variance) -> Variance {
        match (self, inner) {
            (Variance::Invariant, _) | (_, Variance::Invariant) => Variance::Invariant,
            (Variance::Covariant, inner) => inner,
            (Variance::Contravariant, Variance::Covariant) => Variance::Contravariant,
            (Variance::Contravariant, Variance::Contravariant) => Variance::Covariant,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Rest {
    All,
    Positional,
    Keyed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Binding {
    Positional,
    Keyword(SymbolId),
    Rest(Rest),
    /// An implicit ambient binder, which no type application fills
    Implicit,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Binder {
    pub(crate) kind: Kind,
    pub(crate) binding: Binding,
    /// Elaborated upper bound, with the same kind as this binder.
    pub(crate) bound: Option<TypeId>,
    pub(crate) default: Option<TypeId>,
    pub(crate) variance: Variance,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Argument {
    Positional(TypeId),
    Keyword(SymbolId, TypeId),
    Expand(TypeId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Multiplicity {
    Required,
    Optional,
    Repeated,
}

impl Multiplicity {
    /// An item of multiplicity `inner` within one of multiplicity `self`
    pub(crate) fn compose(self, inner: Multiplicity) -> Multiplicity {
        match (self, inner) {
            (Multiplicity::Required, inner) => inner,
            (Multiplicity::Optional, Multiplicity::Repeated) | (Multiplicity::Repeated, _) => {
                Multiplicity::Repeated
            }
            (Multiplicity::Optional, _) => Multiplicity::Optional,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Element {
    Positional(TypeId),
    /// An exact key is a singleton literal type; a key domain is an ordinary type.
    Keyed {
        key: TypeId,
        value: TypeId,
    },
    Include(TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SchemaItem {
    pub(crate) multiplicity: Multiplicity,
    pub(crate) element: Element,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Function {
    pub(crate) params: TypeId,
    pub(crate) result: TypeId,
    /// Absence means no explicit ambient-channel declaration, not an inference hole.
    pub(crate) input: Option<TypeId>,
    pub(crate) output: Option<TypeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum UnionMember {
    Type(TypeId),
    /// A schema's positional items' types, as `Union[...S]` expands it
    Expand(TypeId),
    /// A schema's keys, as `Keys[S]`
    Keys(TypeId),
    /// A schema's items' values, positional or keyed, as `Values[S]`
    Values(TypeId),
    /// A `Tuple[key, value]` for each of a schema's items, as `Entries[S]`
    Entries(TypeId),
    /// The join of the values of a schema's items a key selects, as
    /// `IndexItem[S, K]`
    IndexItem(TypeId, TypeId),
    /// The meet of the values of a schema's items a key selects, as
    /// `AssignItem[S, K]`
    AssignItem(TypeId, TypeId),
}

impl UnionMember {
    /// The member's type, or the schema it projects
    pub(crate) fn id(self) -> TypeId {
        let (Self::Type(id)
        | Self::Expand(id)
        | Self::Keys(id)
        | Self::Values(id)
        | Self::Entries(id)
        | Self::IndexItem(id, _)
        | Self::AssignItem(id, _)) = self;
        id
    }

    /// The key an item projection selects by
    pub(crate) fn key(self) -> Option<TypeId> {
        match self {
            Self::IndexItem(_, key) | Self::AssignItem(_, key) => Some(key),
            _ => None,
        }
    }

    /// The schema it projects, if it's a projection
    pub(crate) fn projected(self) -> Option<TypeId> {
        match self {
            Self::Type(_) => None,
            _ => Some(self.id()),
        }
    }

    /// The same kind of member of another type or schema
    pub(crate) fn with(self, id: TypeId) -> Self {
        match self {
            Self::Type(_) => Self::Type(id),
            Self::Expand(_) => Self::Expand(id),
            Self::Keys(_) => Self::Keys(id),
            Self::Values(_) => Self::Values(id),
            Self::Entries(_) => Self::Entries(id),
            Self::IndexItem(_, key) => Self::IndexItem(id, key),
            Self::AssignItem(_, key) => Self::AssignItem(id, key),
        }
    }

    /// The same member with its key replaced, if it's an item projection
    pub(crate) fn with_key(self, key: TypeId) -> Self {
        match self {
            Self::IndexItem(schema, _) => Self::IndexItem(schema, key),
            Self::AssignItem(schema, _) => Self::AssignItem(schema, key),
            _ => self,
        }
    }
}

/// What a projection member of a union stands for (see [`Database::project`])
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Projected {
    /// The members it reduces to
    Reduced(Vec<UnionMember>),
    /// Its schema isn't known well enough yet
    Pending,
    /// Its schema's keyed view has a key that may be a position's index
    Conflict,
}

/// A schema's keyed view (see [`Database::promoted`])
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Promotion {
    Promoted(Promoted),
    /// The schema isn't known well enough yet
    Pending,
    /// A key may be a position's index
    Conflict,
}

/// A schema's items as a collection indexes them, each position keyed by its
/// index
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Promoted {
    /// The value of each position whose index is fixed, in order
    pub(crate) fixed: Vec<TypeId>,
    /// The values of the positions after them, whose indexes vary
    pub(crate) varying: Vec<TypeId>,
    /// Each keyed item's multiplicity, through inclusions, key and value
    pub(crate) keyed: Vec<(Multiplicity, TypeId, TypeId)>,
    /// The included schemas not yet known
    pub(crate) opaque: Vec<TypeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Type {
    Top,
    /// The dynamic type or schema, consistent with every type or schema of its kind
    Unknown(Kind),
    /// A written type the database can't represent yet. Judgments involving it
    /// are unsupported rather than consistent, so it never passes for `Unknown`.
    /// Each is unique (see [`Database::unsupported`]), since what it stands for
    /// can't be compared.
    #[expect(dead_code, reason = "no written type needs a stand-in now")]
    Unsupported {
        kind: Kind,
        occurrence: u32,
    },
    /// A literal type written in a type, or derived from one, which is exact
    Literal(Literal),
    /// A literal type a literal term gave, which decays to its class where a
    /// variable or collection holds it (see [`Database::decay`]). It relates as its
    /// regular twin does.
    Fresh(Literal),
    Decl(DeclId),
    Bound {
        reference: BoundRef,
        kind: Kind,
    },
    /// Binder `slot` of a declaration's group, held abstract while that declaration
    /// is checked. Closed, unlike a reference; never part of a declaration.
    Rigid {
        decl: DeclId,
        slot: u16,
        kind: Kind,
    },
    /// The result kind is supplied by elaboration; argument matching is deferred.
    Apply {
        base: TypeId,
        args: alias::Box<[Argument]>,
        kind: Kind,
    },
    Union(alias::Box<[UnionMember]>),
    Function(Function),
    Schema(alias::Box<[SchemaItem]>),
    Quantified {
        binders: alias::Box<[Binder]>,
        body: TypeId,
    },
    /// A schema of a type pattern for each item of its packs, as a rest's `@...P`
    /// is. The pattern is interpreted in a group of one type for each pack: slot
    /// `i` is pack `i`'s item. Several packs correspond item by item. Each item has
    /// its pack's multiplicity, and a keyed item its key. Interning reduces it to a
    /// schema once its packs are known (see [`Database::normalize`]).
    Map {
        packs: alias::Box<[TypeId]>,
        pattern: TypeId,
    },
    /// An overloaded function's value: its `@def` signatures, one of which a call
    /// or a function type it's passed as chooses, and its implementation's
    /// signature, which it relates as anywhere else. Never written; a solver
    /// exposes an overloaded def as it, and flow builds it from a method's
    /// signatures. `function` is the declaration they're the signatures of, if
    /// known: its overloads are these, in order, which diagnostics name binders
    /// by.
    Overloaded {
        overloads: alias::Box<[TypeId]>,
        implementation: Option<TypeId>,
        function: Option<DeclId>,
    },
}

impl Type {
    /// Visit immediate children without rebuilding the node. `groups` counts
    /// quantifier boundaries crossed, including those around bounds/defaults.
    pub(crate) fn visit_children(&self, mut visit: impl FnMut(TypeId, u32)) {
        match self {
            Self::Top
            | Self::Unknown(_)
            | Self::Unsupported { .. }
            | Self::Literal(_)
            | Self::Fresh(_)
            | Self::Decl(_)
            | Self::Bound { .. }
            | Self::Rigid { .. } => {}
            Self::Apply { base, args, .. } => {
                visit(*base, 0);
                for arg in args.iter() {
                    let (Argument::Positional(ty)
                    | Argument::Keyword(_, ty)
                    | Argument::Expand(ty)) = arg;
                    visit(*ty, 0);
                }
            }
            Self::Union(members) => {
                for member in members.iter() {
                    visit(member.id(), 0);
                    if let Some(key) = member.key() {
                        visit(key, 0);
                    }
                }
            }
            Self::Function(func) => {
                visit(func.params, 0);
                visit(func.result, 0);
                for ty in func.input.iter().chain(func.output.iter()) {
                    visit(*ty, 0);
                }
            }
            Self::Schema(items) => {
                for item in items.iter() {
                    match item.element {
                        Element::Positional(ty) | Element::Include(ty) => visit(ty, 0),
                        Element::Keyed { key, value } => {
                            visit(key, 0);
                            visit(value, 0);
                        }
                    }
                }
            }
            Self::Quantified { binders, body } => {
                let groups = u32::from(!binders.is_empty());
                for binder in binders.iter() {
                    for ty in binder.bound.iter().chain(binder.default.iter()) {
                        visit(*ty, groups);
                    }
                }
                visit(*body, groups);
            }
            Self::Map { packs, pattern } => {
                for pack in packs.iter() {
                    visit(*pack, 0);
                }
                visit(*pattern, 1);
            }
            Self::Overloaded {
                overloads,
                implementation,
                ..
            } => {
                for ty in overloads.iter().chain(implementation.iter()) {
                    visit(*ty, 0);
                }
            }
        }
    }

    /// Compare all structure except child IDs. Matching shapes have corresponding
    /// children in `visit_children` order with the same binder-depth increments.
    pub(crate) fn same_shape(&self, other: &Self) -> bool {
        use std::mem::discriminant;
        match (self, other) {
            (
                Self::Apply {
                    args: a, kind: ak, ..
                },
                Self::Apply {
                    args: b, kind: bk, ..
                },
            ) => {
                ak == bk
                    && a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|(a, b)| match (a, b) {
                        (Argument::Keyword(a, _), Argument::Keyword(b, _)) => a == b,
                        _ => discriminant(a) == discriminant(b),
                    })
            }
            (Self::Union(a), Self::Union(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(a, b)| discriminant(a) == discriminant(b))
            }
            (Self::Function(a), Self::Function(b)) => {
                a.input.is_some() == b.input.is_some() && a.output.is_some() == b.output.is_some()
            }
            (Self::Schema(a), Self::Schema(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|(a, b)| {
                        a.multiplicity == b.multiplicity
                            && discriminant(&a.element) == discriminant(&b.element)
                    })
            }
            (Self::Quantified { binders: a, .. }, Self::Quantified { binders: b, .. }) => {
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|(a, b)| {
                        a.kind == b.kind
                            && a.binding == b.binding
                            && a.variance == b.variance
                            && a.bound.is_some() == b.bound.is_some()
                            && a.default.is_some() == b.default.is_some()
                    })
            }
            (Self::Map { packs: a, .. }, Self::Map { packs: b, .. }) => a.len() == b.len(),
            (
                Self::Overloaded {
                    overloads: a,
                    implementation: ai,
                    function: af,
                },
                Self::Overloaded {
                    overloads: b,
                    implementation: bi,
                    function: bf,
                },
            ) => a.len() == b.len() && ai.is_some() == bi.is_some() && af == bf,
            _ => self == other,
        }
    }

    /// Rebuild a node by mapping its immediate structural children. `groups` is the number of quantifier
    /// boundaries crossed before visiting that child, including bounds/defaults.
    pub(crate) fn map_children<E>(
        &self,
        mut f: impl FnMut(TypeId, u32) -> Result<TypeId, E>,
    ) -> Result<Self, E> {
        let mut mapped = self.clone();
        match &mut mapped {
            Self::Top
            | Self::Unknown(_)
            | Self::Unsupported { .. }
            | Self::Literal(_)
            | Self::Fresh(_)
            | Self::Decl(_)
            | Self::Bound { .. }
            | Self::Rigid { .. } => {}
            Self::Apply { base, args, .. } => {
                *base = f(*base, 0)?;
                for arg in args.iter_mut() {
                    let (Argument::Positional(ty)
                    | Argument::Keyword(_, ty)
                    | Argument::Expand(ty)) = arg;
                    *ty = f(*ty, 0)?;
                }
            }
            Self::Union(members) => {
                for member in members.iter_mut() {
                    *member = member.with(f(member.id(), 0)?);
                    if let Some(key) = member.key() {
                        *member = member.with_key(f(key, 0)?);
                    }
                }
            }
            Self::Function(func) => {
                func.params = f(func.params, 0)?;
                func.result = f(func.result, 0)?;
                for ty in func.input.iter_mut().chain(func.output.iter_mut()) {
                    *ty = f(*ty, 0)?;
                }
            }
            Self::Schema(items) => {
                for item in items.iter_mut() {
                    match &mut item.element {
                        Element::Positional(ty) | Element::Include(ty) => *ty = f(*ty, 0)?,
                        Element::Keyed { key, value } => {
                            *key = f(*key, 0)?;
                            *value = f(*value, 0)?;
                        }
                    }
                }
            }
            Self::Quantified { binders, body } => {
                let groups = u32::from(!binders.is_empty());
                for binder in binders.iter_mut() {
                    if let Some(ty) = &mut binder.bound {
                        *ty = f(*ty, groups)?;
                    }
                    if let Some(ty) = &mut binder.default {
                        *ty = f(*ty, groups)?;
                    }
                }
                *body = f(*body, groups)?;
            }
            Self::Map { packs, pattern } => {
                for pack in packs.iter_mut() {
                    *pack = f(*pack, 0)?;
                }
                *pattern = f(*pattern, 1)?;
            }
            Self::Overloaded {
                overloads,
                implementation,
                ..
            } => {
                for ty in overloads.iter_mut().chain(implementation.iter_mut()) {
                    *ty = f(*ty, 0)?;
                }
            }
        }
        Ok(mapped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UnitSpan {
    pub(crate) unit: UnitId,
    pub(crate) span: Span,
}

/// Where a slot of a declaration's outer group came from
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinderOrigin {
    /// A binder of an enclosing declaration, which diagnostics hide
    Lifted,
    Written,
    /// A signature's omitted ambient channel, which diagnostics hide
    Implicit,
}

#[derive(Clone, Debug)]
pub(crate) struct BinderSource {
    pub(crate) name: SymbolId,
    #[expect(
        dead_code,
        reason = "diagnostics don't cite a binder's declaration yet"
    )]
    pub(crate) span: UnitSpan,
    pub(crate) bound: Option<UnitSpan>,
    pub(crate) default: Option<UnitSpan>,
    pub(crate) origin: BinderOrigin,
}

/// Which namespace a class member belongs to
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Instance,
    /// The type object's, inherited by subclasses
    Class,
    /// The type object's, not inherited
    Static,
}

/// A member's name. A special method such as `(init)` is named without its
/// parentheses, apart from an ordinary member of the same name. A private member
/// is named apart from public ones, as the runtime names it by a symbol of its
/// class's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MemberKey {
    pub(crate) name: SymbolId,
    pub(crate) special: bool,
    pub(crate) private: bool,
}

/// A class member. Methods are function declarations, lifted over all of the
/// class's binders.
#[derive(Clone, Debug)]
pub(crate) enum Member {
    /// Its type is interpreted in the scope of the class's outer binder group.
    Field {
        ty: TypeId,
        scope: Scope,
        public: bool,
    },
    Method {
        decl: DeclId,
        scope: Scope,
        public: bool,
    },
    /// A computed field, read and written through methods
    Property {
        getter: Option<DeclId>,
        setter: Option<DeclId>,
        scope: Scope,
        public: bool,
    },
    /// A method its decorators replace with a value of unknown type
    Decorated {
        decl: DeclId,
        scope: Scope,
        public: bool,
    },
}

impl Member {
    pub(crate) fn scope(&self) -> Scope {
        match *self {
            Self::Field { scope, .. }
            | Self::Method { scope, .. }
            | Self::Property { scope, .. }
            | Self::Decorated { scope, .. } => scope,
        }
    }

    pub(crate) fn public(&self) -> bool {
        match *self {
            Self::Field { public, .. }
            | Self::Method { public, .. }
            | Self::Property { public, .. }
            | Self::Decorated { public, .. } => public,
        }
    }

    /// The function declarations it holds
    pub(crate) fn decls(&self) -> impl Iterator<Item = DeclId> {
        let (first, second) = match *self {
            Self::Field { .. } => (None, None),
            Self::Method { decl, .. } | Self::Decorated { decl, .. } => (Some(decl), None),
            Self::Property { getter, setter, .. } => (getter, setter),
        };
        first.into_iter().chain(second)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DeclKind {
    Class,
    Protocol,
    OpaqueAlias,
    Alias,
    Function,
    Closure,
    Annotation,
}

impl DeclKind {
    pub(crate) fn nominal(self) -> bool {
        matches!(self, Self::Class | Self::Protocol | Self::OpaqueAlias)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DeclSource {
    pub(crate) kind: DeclKind,
    pub(crate) result_kind: Kind,
    pub(crate) name: Option<SymbolId>,
    pub(crate) span: UnitSpan,
}

/// A nominal declaration's supertype
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Supertype {
    /// Interpreted in the scope of the declaration's outer binder group, when
    /// present.
    pub(crate) ty: TypeId,
    /// Whether the runtime inherits from it. A class's `@` supertypes, and all
    /// of a protocol's, are only claims the type checker holds it to.
    pub(crate) runtime: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Declaration {
    pub(crate) source: DeclSource,
    pub(crate) ty: TypeId,
    pub(crate) binders: alias::Box<[BinderSource]>,
    pub(crate) supertypes: alias::Box<[Supertype]>,
    /// A class's or protocol's members, in source order
    pub(crate) members: alias::Box<[(MemberKey, Member)]>,
}

enum Declarations {
    Building(Vec<Option<Declaration>>),
    Frozen(alias::Box<[Declaration]>),
}

impl Declarations {
    fn get(&self, id: DeclId) -> Option<&Declaration> {
        match self {
            Self::Building(slots) => slots[id.index()].as_ref(),
            Self::Frozen(declarations) => Some(&declarations[id.index()]),
        }
    }

    fn building_mut(&mut self) -> &mut Vec<Option<Declaration>> {
        match self {
            Self::Building(slots) => slots,
            Self::Frozen(_) => panic!("declaration database is sealed"),
        }
    }
}

/// Transparent declaration chain, including the repeated declaration closing it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
pub(crate) struct ExposureCycle(pub(crate) Vec<DeclId>);

/// A reference that prevents removing its enclosing binder group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemovedBinder(pub(crate) BoundRef);

/// A rigid of a declaration other than the one being abstracted over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Escape(pub(crate) TypeId);

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
pub(crate) struct Exposure {
    pub(crate) ty: TypeId,
    /// Transparent wrappers traversed, in source-to-underlying order.
    pub(crate) declarations: Vec<DeclId>,
}

/// Stub types recognized by elaboration and intrinsic subtype rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intrinsic {
    Union,
    /// `Keys[S]`, the union of a schema's keys
    Keys,
    /// `Values[S]`, the union of a schema's values
    Values,
    /// `Entries[S]`, the union of a schema's keyed items as key-value tuples
    Entries,
    /// `IndexItem[S, K]`, the join of the values of a schema's items a key selects
    IndexItem,
    /// `AssignItem[S, K]`, the meet of the values of a schema's items a key selects
    AssignItem,
    /// The class of a tuple, which `Entries` builds
    Tuple,
    /// The class of function values, and of every function type (see
    /// [`Database::func_class`]). Bare in a type, it is the gradual function type.
    Func,
    Int,
    Bool,
    Sym,
    Nil,
    Str,
    /// `Type[C]`, the type of the class object whose instances are `C`
    Type,
    /// The class whose members every value has, which top's members are
    /// looked up on and every class's lookup falls back to
    Value,
}

/// Optional associations to elaborated stub types, populated before sealing.
#[derive(Default)]
struct Intrinsics {
    union: Option<TypeId>,
    keys: Option<TypeId>,
    values: Option<TypeId>,
    entries: Option<TypeId>,
    index_item: Option<TypeId>,
    assign_item: Option<TypeId>,
    tuple: Option<TypeId>,
    func: Option<TypeId>,
    int: Option<TypeId>,
    bool: Option<TypeId>,
    sym: Option<TypeId>,
    nil: Option<TypeId>,
    str: Option<TypeId>,
    ty: Option<TypeId>,
    value: Option<TypeId>,
}

impl Intrinsics {
    fn get(&self, intrinsic: Intrinsic) -> Option<TypeId> {
        match intrinsic {
            Intrinsic::Union => self.union,
            Intrinsic::Keys => self.keys,
            Intrinsic::Values => self.values,
            Intrinsic::Entries => self.entries,
            Intrinsic::IndexItem => self.index_item,
            Intrinsic::AssignItem => self.assign_item,
            Intrinsic::Tuple => self.tuple,
            Intrinsic::Func => self.func,
            Intrinsic::Int => self.int,
            Intrinsic::Bool => self.bool,
            Intrinsic::Sym => self.sym,
            Intrinsic::Nil => self.nil,
            Intrinsic::Str => self.str,
            Intrinsic::Type => self.ty,
            Intrinsic::Value => self.value,
        }
    }

    fn slot_mut(&mut self, intrinsic: Intrinsic) -> &mut Option<TypeId> {
        match intrinsic {
            Intrinsic::Union => &mut self.union,
            Intrinsic::Keys => &mut self.keys,
            Intrinsic::Values => &mut self.values,
            Intrinsic::Entries => &mut self.entries,
            Intrinsic::IndexItem => &mut self.index_item,
            Intrinsic::AssignItem => &mut self.assign_item,
            Intrinsic::Tuple => &mut self.tuple,
            Intrinsic::Func => &mut self.func,
            Intrinsic::Int => &mut self.int,
            Intrinsic::Bool => &mut self.bool,
            Intrinsic::Sym => &mut self.sym,
            Intrinsic::Nil => &mut self.nil,
            Intrinsic::Str => &mut self.str,
            Intrinsic::Type => &mut self.ty,
            Intrinsic::Value => &mut self.value,
        }
    }
}

pub(crate) struct Database {
    top: TypeId,
    bottom: TypeId,
    unknown: TypeId,
    unknown_schema: TypeId,
    intrinsics: Intrinsics,
    types: intern::Table<Type, TypeTag>,
    symbols: intern::Table<String, SymbolTag>,
    unit_count: usize,
    declarations: Declarations,
    /// Each function's signatures, when it has more than one
    overloads: HashMap<DeclId, alias::Box<[DeclId]>>,
    pending_kinds: RefCell<Vec<(TypeId, Kind)>>,
    /// How many unsupported stand-ins have been interned
    #[expect(dead_code, reason = "no written type needs a stand-in now")]
    unsupported: Cell<u32>,
}

impl Default for Database {
    fn default() -> Self {
        let types = intern::Table::new();
        Self {
            top: types.id_owned(Type::Top),
            bottom: types.id_owned(Type::Union(alias::Box::default())),
            unknown: types.id_owned(Type::Unknown(Kind::Type)),
            unknown_schema: types.id_owned(Type::Unknown(Kind::Schema)),
            intrinsics: Intrinsics::default(),
            types,
            symbols: intern::Table::new(),
            unit_count: 0,
            declarations: Declarations::Building(Vec::new()),
            overloads: HashMap::new(),
            pending_kinds: RefCell::new(Vec::new()),
            unsupported: Cell::new(0),
        }
    }
}

impl Database {
    pub(crate) fn is_sealed(&self) -> bool {
        matches!(self.declarations, Declarations::Frozen(_))
    }

    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn top(&self) -> TypeId {
        self.top
    }

    /// The empty union, interned before any source declarations.
    pub(crate) fn bottom(&self) -> TypeId {
        self.bottom
    }

    /// The dynamic type, interned before any source declarations.
    pub(crate) fn unknown(&self) -> TypeId {
        self.unknown
    }

    /// The dynamic schema, interned before any source declarations.
    pub(crate) fn unknown_schema(&self) -> TypeId {
        self.unknown_schema
    }

    /// A new stand-in for a written type the database can't represent, distinct
    /// from every other
    #[expect(dead_code, reason = "no written type needs a stand-in now")]
    pub(crate) fn unsupported(&self, kind: Kind) -> TypeId {
        let occurrence = self.unsupported.get();
        self.unsupported.set(occurrence + 1);
        self.intern(Type::Unsupported { kind, occurrence })
    }

    /// The dynamic type or schema of a kind
    pub(crate) fn unknown_of(&self, kind: Kind) -> TypeId {
        match kind {
            Kind::Type => self.unknown,
            Kind::Schema => self.unknown_schema,
        }
    }

    /// The `@def` signatures of an overloaded function, in source order. Empty for
    /// a function with one signature. The function's own ID is its implementation,
    /// unless it's among these: then it has none (see [`Self::implementation`]).
    pub(crate) fn overloads(&self, id: DeclId) -> &[DeclId] {
        self.overloads.get(&id).map_or(&[], |overloads| overloads)
    }

    /// A function's implementation signature: its own ID, unless it's overloaded
    /// without one
    pub(crate) fn implementation(&self, id: DeclId) -> Option<DeclId> {
        (!self.overloads(id).contains(&id)).then_some(id)
    }

    /// Record the `@def` signatures of an overloaded function once, before
    /// sealing.
    pub(crate) fn set_overloads(&mut self, id: DeclId, overloads: Vec<DeclId>) {
        self.require_open();
        assert!(
            !overloads.is_empty(),
            "an overloaded function has overloads"
        );
        assert!(
            self.overloads.insert(id, overloads.into()).is_none(),
            "overloads already set: {id:?}"
        );
    }

    pub(crate) fn intrinsic(&self, intrinsic: Intrinsic) -> Option<TypeId> {
        self.intrinsics.get(intrinsic)
    }

    /// The function type of any function, as bare `Func` is: `(...Unknown) ->
    /// Unknown`, with its ambient channels omitted
    pub(crate) fn gradual_function(&self) -> TypeId {
        self.intern(Type::Function(Function {
            params: self.unknown_schema(),
            result: self.unknown(),
            input: None,
            output: None,
        }))
    }

    /// The class of a function type's values, `Func`, or `None` if the type isn't
    /// a function, quantified or not
    pub(crate) fn func_class(&self, mut ty: TypeId) -> Option<TypeId> {
        let base = self.intrinsic(Intrinsic::Func)?;
        while let Type::Quantified { body, .. } = self.ty(ty) {
            ty = *body;
        }
        matches!(self.ty(ty), Type::Function(_)).then_some(base)
    }

    /// Associate a stub type once, before sealing. Missing associations are allowed.
    pub(crate) fn set_intrinsic(&mut self, intrinsic: Intrinsic, ty: TypeId) {
        self.require_open();
        assert!(
            self.intrinsic(intrinsic).is_none(),
            "intrinsic already set: {intrinsic:?}"
        );
        self.expect_kind(ty, Kind::Type);
        *self.intrinsics.slot_mut(intrinsic) = Some(ty);
    }

    pub(crate) fn ty(&self, id: TypeId) -> &Type {
        &self.types[id]
    }

    /// The literal a literal type is, fresh or regular
    pub(crate) fn literal(&self, id: TypeId) -> Option<&Literal> {
        match self.ty(id) {
            Type::Literal(literal) | Type::Fresh(literal) => Some(literal),
            _ => None,
        }
    }

    /// A fresh literal's regular twin, or any other type itself
    pub(crate) fn regular(&self, id: TypeId) -> TypeId {
        match self.ty(id) {
            Type::Fresh(literal) => self.intern(Type::Literal(literal.clone())),
            _ => id,
        }
    }

    /// Every declaration of a sealed database
    pub(crate) fn declarations(&self) -> impl Iterator<Item = (DeclId, &Declaration)> {
        let Declarations::Frozen(declarations) = &self.declarations else {
            panic!("declaration database is not sealed");
        };
        declarations
            .iter()
            .enumerate()
            .map(|(index, declaration)| (DeclId::from_index(index), declaration))
    }

    pub(crate) fn declaration(&self, id: DeclId) -> &Declaration {
        self.declarations.get(id).expect("unpopulated declaration")
    }

    pub(crate) fn symbol(&self, id: SymbolId) -> &str {
        &self.symbols[id]
    }

    pub(crate) fn intern_symbol(&self, text: &str) -> SymbolId {
        self.symbols.id(text)
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn fresh_symbol(&self, text: &str) -> SymbolId {
        self.symbols.fresh(text.into())
    }

    pub(crate) fn allocate_unit(&mut self) -> UnitId {
        self.require_open();
        let id = UnitId::from_index(self.unit_count);
        self.unit_count += 1;
        id
    }

    pub(crate) fn allocate(&mut self) -> DeclId {
        let slots = self.declarations.building_mut();
        let id = DeclId::from_index(slots.len());
        slots.push(None);
        id
    }

    pub(crate) fn populate(&mut self, id: DeclId, declaration: Declaration) {
        self.require_open();
        assert!(
            self.declarations.get(id).is_none(),
            "declaration already populated: {id:?}"
        );
        self.validate_declaration(&declaration);
        self.declarations.building_mut()[id.index()] = Some(declaration);
    }

    /// Replace a sealed declaration's type, validating it as population does.
    /// Sealing closes the set of declarations; a checked one may still be refined.
    pub(crate) fn retype(&mut self, id: DeclId, ty: TypeId) {
        let Declarations::Frozen(declarations) = &mut self.declarations else {
            panic!("declaration database is not sealed");
        };
        declarations[id.index()].ty = ty;
        self.validate_declaration(self.declaration(id));
        assert!(
            self.pending_kinds.borrow().is_empty(),
            "every kind is known once sealed"
        );
    }

    fn validate_declaration(&self, declaration: &Declaration) {
        assert!(
            declaration.source.span.unit.index() < self.unit_count,
            "unallocated unit ID"
        );
        self.expect_kind(declaration.ty, declaration.source.result_kind);
        let arity = match self.ty(declaration.ty) {
            Type::Quantified { binders, .. } => binders.len(),
            _ => 0,
        };
        assert_eq!(
            arity,
            declaration.binders.len(),
            "binder metadata arity mismatch"
        );
        assert!(
            declaration.source.kind.nominal() || declaration.supertypes.is_empty(),
            "unexpected supertypes on transparent declaration"
        );
        for supertype in declaration.supertypes.iter() {
            self.expect_kind(supertype.ty, Kind::Type);
        }
        assert!(
            matches!(
                declaration.source.kind,
                DeclKind::Class | DeclKind::Protocol
            ) || declaration.members.is_empty(),
            "unexpected members on a declaration that is not a class"
        );
        for (_, member) in declaration.members.iter() {
            if let Member::Field { ty, .. } = member {
                self.expect_kind(*ty, Kind::Type);
            }
        }
        let fields = declaration
            .members
            .iter()
            .filter_map(|(_, member)| match member {
                Member::Field { ty, .. } => Some(*ty),
                _ => None,
            });
        for root in [declaration.ty]
            .into_iter()
            .chain(declaration.supertypes.iter().map(|supertype| supertype.ty))
            .chain(fields)
        {
            self.walk(root, |id, _| {
                assert!(
                    !matches!(self.ty(id), Type::Rigid { .. }),
                    "rigid in a declaration"
                );
            });
        }
    }

    pub(crate) fn seal(&mut self) {
        let Declarations::Building(slots) = &self.declarations else {
            panic!("declaration database is sealed");
        };
        for (index, slot) in slots.iter().enumerate() {
            assert!(
                slot.is_some(),
                "unpopulated declaration: {:?}",
                DeclId::from_index(index)
            );
        }
        for (index, slot) in slots.iter().enumerate() {
            let declaration = slot.as_ref().unwrap();
            for decl in declaration
                .members
                .iter()
                .flat_map(|(_, member)| member.decls())
            {
                assert!(
                    matches!(
                        slots.get(decl.index()),
                        Some(Some(Declaration {
                            source: DeclSource {
                                kind: DeclKind::Function,
                                ..
                            },
                            ..
                        }))
                    ),
                    "method of {:?} is not a function",
                    DeclId::from_index(index)
                );
            }
        }
        for (id, overloads) in &self.overloads {
            for overload in overloads.iter().chain([id]) {
                assert!(
                    matches!(
                        slots.get(overload.index()),
                        Some(Some(Declaration {
                            source: DeclSource {
                                kind: DeclKind::Function,
                                ..
                            },
                            ..
                        }))
                    ),
                    "overload of {id:?} is not a function"
                );
            }
        }
        // Keep these checks even when normalization discarded the original node.
        for &(ty, expected) in self.pending_kinds.borrow().iter() {
            assert_eq!(self.kind(ty), expected, "type kind mismatch");
        }
        self.pending_kinds.get_mut().clear();
        let declarations = std::mem::take(self.declarations.building_mut())
            .into_iter()
            .map(Option::unwrap)
            .collect();
        self.declarations = Declarations::Frozen(declarations);
    }

    fn require_open(&self) {
        assert!(
            matches!(self.declarations, Declarations::Building(_)),
            "declaration database is sealed"
        );
    }

    pub(crate) fn kind(&self, id: TypeId) -> Kind {
        self.known_kind(id)
            .expect("kind of unpopulated declaration")
    }

    fn known_kind(&self, id: TypeId) -> Option<Kind> {
        match self.ty(id) {
            Type::Schema(_) | Type::Map { .. } => Some(Kind::Schema),
            Type::Bound { kind, .. } | Type::Rigid { kind, .. } | Type::Apply { kind, .. } => {
                Some(*kind)
            }
            Type::Decl(id) => self
                .declarations
                .get(*id)
                .map(|decl| decl.source.result_kind),
            Type::Quantified { body, .. } => self.known_kind(*body),
            Type::Unknown(kind) | Type::Unsupported { kind, .. } => Some(*kind),
            _ => Some(Kind::Type),
        }
    }

    fn expect_kind(&self, id: TypeId, expected: Kind) {
        if let Some(actual) = self.known_kind(id) {
            assert_eq!(actual, expected, "type kind mismatch");
        } else {
            self.pending_kinds.borrow_mut().push((id, expected));
        }
    }

    pub(crate) fn intern(&self, ty: Type) -> TypeId {
        self.validate(&ty);
        let ty = self.normalize(ty);
        self.types.id_owned(ty)
    }

    /// A type's canonical outer form, which interning gives it: a quantifier
    /// without binders is its body, and an application of the `Union`, `Keys`,
    /// `Values` or `Entries` intrinsic is a union of its projection. A union is
    /// flattened, with its members sorted and deduplicated, and each projection of
    /// a schema among them is evaluated as far as the schema is known (see
    /// [`Self::project`]). A union with `Top` is `Top`, and one with a single
    /// member is it. A mapping whose packs are known is reduced (see
    /// [`Self::reduce_map`]). Children are assumed canonical already.
    pub(crate) fn normalize(&self, ty: Type) -> Type {
        match ty {
            Type::Quantified { binders, body } if binders.is_empty() => self.ty(body).clone(),
            Type::Map { packs, pattern } => match self.reduce_map(&packs, pattern) {
                Some(reduced) => reduced,
                None => Type::Map { packs, pattern },
            },
            Type::Apply { base, args, kind } => match self.projection(base, &args) {
                Some(members) => self.normalize(Type::Union(members.into())),
                None => Type::Apply { base, args, kind },
            },
            Type::Union(members) => {
                let mut normalized = Vec::new();
                let mut pending: Vec<UnionMember> = members.iter().rev().copied().collect();
                while let Some(member) = pending.pop() {
                    match member {
                        UnionMember::Type(id) => match self.ty(id) {
                            Type::Top => return Type::Top,
                            Type::Union(nested) => normalized.extend_from_slice(nested),
                            _ => normalized.push(member),
                        },
                        _ => match self.project(member) {
                            Projected::Reduced(members) => {
                                pending.extend(members.into_iter().rev())
                            }
                            Projected::Pending | Projected::Conflict => normalized.push(member),
                        },
                    }
                }
                normalized.sort_unstable();
                normalized.dedup();
                // A fresh literal is its regular twin's, which is kept
                let regular: Vec<TypeId> = (normalized.iter())
                    .filter_map(|member| match *member {
                        UnionMember::Type(id) if matches!(self.ty(id), Type::Literal(_)) => {
                            Some(id)
                        }
                        _ => None,
                    })
                    .collect();
                if !regular.is_empty() {
                    normalized.retain(|member| match *member {
                        UnionMember::Type(id) if matches!(self.ty(id), Type::Fresh(_)) => {
                            !regular.contains(&self.regular(id))
                        }
                        _ => true,
                    });
                }
                match normalized.as_slice() {
                    [UnionMember::Type(id)] => self.ty(*id).clone(),
                    _ => Type::Union(normalized.into()),
                }
            }
            ty => ty,
        }
    }

    /// The union members an application of a projecting intrinsic stands for, one
    /// for each schema argument
    fn projection(&self, base: TypeId, args: &[Argument]) -> Option<Vec<UnionMember>> {
        for (intrinsic, member) in [
            (
                Intrinsic::IndexItem,
                UnionMember::IndexItem as fn(TypeId, TypeId) -> UnionMember,
            ),
            (Intrinsic::AssignItem, UnionMember::AssignItem),
        ] {
            if self.intrinsic(intrinsic) == Some(base) {
                return match *args {
                    [Argument::Positional(schema), Argument::Positional(key)] => {
                        Some(vec![member(schema, key)])
                    }
                    _ => None,
                };
            }
        }
        let member = [
            (
                Intrinsic::Union,
                UnionMember::Expand as fn(TypeId) -> UnionMember,
            ),
            (Intrinsic::Keys, UnionMember::Keys),
            (Intrinsic::Values, UnionMember::Values),
            (Intrinsic::Entries, UnionMember::Entries),
        ]
        .into_iter()
        .find(|&(intrinsic, _)| self.intrinsic(intrinsic) == Some(base))?
        .1;
        (args.iter())
            .map(|arg| match *arg {
                Argument::Positional(schema) | Argument::Expand(schema) => Some(member(schema)),
                Argument::Keyword(..) => None,
            })
            .collect()
    }

    /// What a projection member of a union stands for, as far as its schema is
    /// known, or `None` to keep it. `Union[...S]` expands only a schema of
    /// positional items into their types, and projects an included schema the same
    /// way in turn, so a known item is reduced beside a pack. `Values` folds each
    /// item's value, ignoring its multiplicity, and projects an included schema
    /// the same way in turn. `Keys` and `Entries` fold the schema's keyed view
    /// (see [`Self::promoted`]): `Keys` takes each key, and `Entries` each
    /// `Tuple[key, value]`. A schema not yet known keeps the projection, and the
    /// dynamic schema projects to the dynamic type. `Entries` stays whole without
    /// a designated `Tuple`, and a projection with a varying position without a
    /// designated `Int`.
    pub(crate) fn project(&self, member: UnionMember) -> Projected {
        let Some(schema) = member.projected() else {
            return Projected::Pending;
        };
        if let UnionMember::Expand(_) = member {
            let Type::Schema(items) = self.ty(schema) else {
                return Projected::Pending;
            };
            return (items.iter())
                .map(|item| match item.element {
                    Element::Positional(ty) => Some(UnionMember::Type(ty)),
                    Element::Include(inner) => Some(member.with(inner)),
                    Element::Keyed { .. } => None,
                })
                .collect::<Option<_>>()
                .map_or(Projected::Pending, Projected::Reduced);
        }
        let items = match self.ty(schema) {
            Type::Unknown(_) => return Projected::Reduced(vec![UnionMember::Type(self.unknown)]),
            // Selecting by a key takes the solver
            _ if member.key().is_some() => return Projected::Pending,
            Type::Schema(items) => items,
            _ => return Projected::Pending,
        };
        if let UnionMember::Values(_) = member {
            let members = (items.iter())
                .map(|item| match item.element {
                    Element::Include(inner) => member.with(inner),
                    Element::Positional(ty) | Element::Keyed { value: ty, .. } => {
                        UnionMember::Type(ty)
                    }
                })
                .collect();
            return Projected::Reduced(members);
        }
        let promoted = match self.promoted(schema) {
            Promotion::Promoted(promoted) => promoted,
            Promotion::Pending => return Projected::Pending,
            Promotion::Conflict => return Projected::Conflict,
        };
        let int = self.intrinsic(Intrinsic::Int);
        let index = |i: usize| self.intern(Type::Literal(Literal::Int(i as i128)));
        let mut members = Vec::new();
        match member {
            UnionMember::Keys(_) => {
                match (promoted.varying.is_empty(), int) {
                    (true, _) => members
                        .extend((0..promoted.fixed.len()).map(|i| UnionMember::Type(index(i)))),
                    (false, Some(int)) => members.push(UnionMember::Type(int)),
                    (false, None) => return Projected::Pending,
                }
                members.extend(
                    promoted
                        .keyed
                        .iter()
                        .map(|&(_, key, _)| UnionMember::Type(key)),
                );
            }
            _ => {
                let mut pairs: Vec<(TypeId, TypeId)> = (promoted.fixed.iter().enumerate())
                    .map(|(i, &value)| (index(i), value))
                    .collect();
                if !promoted.varying.is_empty() {
                    let Some(int) = int else {
                        return Projected::Pending;
                    };
                    pairs.extend(promoted.varying.iter().map(|&value| (int, value)));
                }
                pairs.extend(promoted.keyed.iter().map(|&(_, key, value)| (key, value)));
                for (key, value) in pairs {
                    let Some(entry) = self.entry(key, value) else {
                        return Projected::Pending;
                    };
                    members.push(UnionMember::Type(entry));
                }
            }
        }
        members.extend(promoted.opaque.iter().map(|&inner| member.with(inner)));
        Projected::Reduced(members)
    }

    /// A schema's keyed view, which a collection indexes: each position is keyed
    /// by its index, through inclusions. The positions before the first that may
    /// be missing or repeated have fixed indexes; the rest vary, as do those
    /// after an included schema not yet known. Such a schema stays opaque, unless
    /// it's alongside positions or a key that may be an index, since the view
    /// depends on its positions: then the view is pending.
    ///
    /// A key that may be the index of a position conflicts with it: a
    /// non-negative `Int` literal that a position may have, or a domain of `Int`
    /// alongside positions. So does a union with such a member. Whether any
    /// other domain holds an index isn't decided here, so it doesn't conflict.
    pub(crate) fn promoted(&self, schema: TypeId) -> Promotion {
        let mut promoted = Promoted::default();
        let mut varies = false;
        let mut pending = false;
        if !self.promote_into(
            schema,
            Multiplicity::Required,
            &mut promoted,
            &mut varies,
            &mut pending,
        ) {
            return Promotion::Pending;
        }
        let positions = !promoted.fixed.is_empty() || !promoted.varying.is_empty();
        let collides = |key| self.collides(key, promoted.fixed.len(), !promoted.varying.is_empty());
        if (promoted.keyed.iter()).any(|&(_, key, _)| collides(key)) {
            return Promotion::Conflict;
        }
        if pending
            && (positions
                || promoted
                    .keyed
                    .iter()
                    .any(|&(_, key, _)| self.collides(key, 0, true)))
        {
            return Promotion::Pending;
        }
        Promotion::Promoted(promoted)
    }

    /// Add a schema's items to its keyed view, as items of an item of
    /// `multiplicity`. Returns whether it's a schema.
    fn promote_into(
        &self,
        schema: TypeId,
        multiplicity: Multiplicity,
        promoted: &mut Promoted,
        varies: &mut bool,
        pending: &mut bool,
    ) -> bool {
        let Type::Schema(items) = self.ty(schema) else {
            return false;
        };
        for item in items.iter() {
            let multiplicity = multiplicity.compose(item.multiplicity);
            match item.element {
                Element::Positional(value) => {
                    *varies |= multiplicity != Multiplicity::Required;
                    match *varies {
                        true => promoted.varying.push(value),
                        false => promoted.fixed.push(value),
                    }
                }
                Element::Keyed { key, value } => promoted.keyed.push((multiplicity, key, value)),
                Element::Include(inner) => {
                    if !self.promote_into(inner, multiplicity, promoted, varies, pending) {
                        *pending |= !matches!(self.ty(inner), Type::Unknown(_));
                        *varies = true;
                        promoted.opaque.push(inner);
                    }
                }
            }
        }
        true
    }

    /// Whether a key may be the index of a position, given how many positions
    /// have fixed indexes and whether more follow
    fn collides(&self, key: TypeId, fixed: usize, varying: bool) -> bool {
        match self.ty(key) {
            Type::Literal(Literal::Int(i)) | Type::Fresh(Literal::Int(i)) => {
                *i >= 0 && (varying || usize::try_from(*i).is_ok_and(|i| i < fixed))
            }
            Type::Union(members) => members.iter().any(|member| match *member {
                UnionMember::Type(ty) => self.collides(ty, fixed, varying),
                _ => false,
            }),
            _ => (fixed > 0 || varying) && Some(key) == self.intrinsic(Intrinsic::Int),
        }
    }

    /// `Tuple[key, value]`, if `Tuple` is designated
    fn entry(&self, key: TypeId, value: TypeId) -> Option<TypeId> {
        let tuple = self.intrinsic(Intrinsic::Tuple)?;
        let item = |ty| SchemaItem {
            multiplicity: Multiplicity::Required,
            element: Element::Positional(ty),
        };
        let items = self.intern(Type::Schema(vec![item(key), item(value)].into()));
        Some(self.intern(Type::Apply {
            base: tuple,
            args: vec![Argument::Positional(items)].into(),
            kind: Kind::Type,
        }))
    }

    /// The schema a mapping stands for, if its packs are known well enough: the
    /// pattern for each item of its packs, with the item's multiplicity and key.
    /// The dynamic pack maps to the dynamic schema. A pack's included schema not
    /// yet known stays a mapping over it, but only a single pack may have one,
    /// since several packs' items correspond only once each is known. Several packs
    /// must agree, item by item, on multiplicity and on whether, and by which key,
    /// the item is keyed; otherwise the mapping stays.
    fn reduce_map(&self, packs: &[TypeId], pattern: TypeId) -> Option<Type> {
        if let [pack] = *packs
            && let Type::Unknown(_) = self.ty(pack)
        {
            return Some(Type::Unknown(Kind::Schema));
        }
        let mut spliced = Vec::with_capacity(packs.len());
        for &pack in packs {
            let mut items = Vec::new();
            if !self.splice(pack, Multiplicity::Required, &mut items) {
                return None;
            }
            spliced.push(items);
        }
        let item = |multiplicity, element| SchemaItem {
            multiplicity,
            element,
        };
        if let [items] = &spliced[..] {
            let items = items.iter().map(|spliced| {
                let element = match spliced.element {
                    Element::Positional(ty) => {
                        Element::Positional(self.instantiate(pattern, &[ty]))
                    }
                    Element::Keyed { key, value } => Element::Keyed {
                        key,
                        value: self.instantiate(pattern, &[value]),
                    },
                    Element::Include(inner) => Element::Include(self.intern(Type::Map {
                        packs: vec![inner].into(),
                        pattern,
                    })),
                };
                item(spliced.multiplicity, element)
            });
            return Some(Type::Schema(items.collect()));
        }
        let (first, rest) = spliced.split_first()?;
        if rest.iter().any(|items| items.len() != first.len()) {
            return None;
        }
        let mut items = Vec::with_capacity(first.len());
        for (index, lead) in first.iter().enumerate() {
            let mut types = Vec::with_capacity(spliced.len());
            for items in &spliced {
                let other = &items[index];
                if other.multiplicity != lead.multiplicity {
                    return None;
                }
                match (&lead.element, &other.element) {
                    (Element::Positional(_), &Element::Positional(ty)) => types.push(ty),
                    (Element::Keyed { key, .. }, &Element::Keyed { key: other, value })
                        if *key == other =>
                    {
                        types.push(value)
                    }
                    _ => return None,
                }
            }
            let ty = self.instantiate(pattern, &types);
            let element = match lead.element {
                Element::Keyed { key, .. } => Element::Keyed { key, value: ty },
                _ => Element::Positional(ty),
            };
            items.push(item(lead.multiplicity, element));
        }
        Some(Type::Schema(items.into()))
    }

    /// Add a schema's items to `items` as items of an item of `multiplicity`,
    /// splicing the schemas it includes that are known. Returns whether it's a
    /// schema.
    fn splice(
        &self,
        schema: TypeId,
        multiplicity: Multiplicity,
        items: &mut Vec<SchemaItem>,
    ) -> bool {
        let Type::Schema(spliced) = self.ty(schema) else {
            return false;
        };
        for item in spliced.iter() {
            let multiplicity = multiplicity.compose(item.multiplicity);
            match item.element {
                Element::Include(inner) if self.splice(inner, multiplicity, items) => {}
                ref element => items.push(SchemaItem {
                    multiplicity,
                    element: element.clone(),
                }),
            }
        }
        true
    }

    fn validate(&self, ty: &Type) {
        match ty {
            Type::Top
            | Type::Unknown(_)
            | Type::Unsupported { .. }
            | Type::Literal(_)
            | Type::Fresh(_)
            | Type::Bound { .. } => {}
            Type::Decl(id) | Type::Rigid { decl: id, .. } => {
                self.declarations.get(*id);
            }
            Type::Apply { base, args, .. } => {
                // A constructor can return either kind. Arity/kind matching of
                // its ordinary arguments requires elaboration, not interning.
                self.ty(*base);
                for arg in args.iter() {
                    match arg {
                        Argument::Expand(id) => self.expect_kind(*id, Kind::Schema),
                        Argument::Positional(id) | Argument::Keyword(_, id) => {
                            self.ty(*id);
                        }
                    }
                }
            }
            Type::Union(members) => {
                for member in members.iter() {
                    match member {
                        UnionMember::Type(id) => self.expect_kind(*id, Kind::Type),
                        _ => {
                            self.expect_kind(member.id(), Kind::Schema);
                            if let Some(key) = member.key() {
                                self.expect_kind(key, Kind::Type);
                            }
                        }
                    }
                }
            }
            Type::Overloaded {
                overloads,
                implementation,
                function,
            } => {
                assert!(!overloads.is_empty(), "an overload set without overloads");
                if let Some(function) = function {
                    self.declarations.get(*function);
                }
                for id in overloads.iter().chain(implementation.iter()) {
                    self.expect_kind(*id, Kind::Type);
                }
            }
            Type::Function(func) => {
                self.expect_kind(func.params, Kind::Schema);
                for id in [Some(func.result), func.input, func.output]
                    .into_iter()
                    .flatten()
                {
                    self.expect_kind(id, Kind::Type);
                }
            }
            Type::Schema(items) => {
                for item in items.iter() {
                    match item.element {
                        Element::Positional(id) => self.expect_kind(id, Kind::Type),
                        Element::Include(id) => self.expect_kind(id, Kind::Schema),
                        Element::Keyed { key, value } => {
                            self.expect_kind(key, Kind::Type);
                            self.expect_kind(value, Kind::Type);
                        }
                    }
                }
            }
            Type::Quantified { binders, body } => {
                assert!(
                    binders.len() <= usize::from(u16::MAX) + 1,
                    "binder count overflow"
                );
                self.ty(*body);
                for binder in binders.iter() {
                    assert!(
                        !matches!(binder.binding, Binding::Rest(_)) || binder.kind == Kind::Schema,
                        "rest binder must have schema kind"
                    );
                    if let Some(id) = binder.bound {
                        self.expect_kind(id, binder.kind);
                    }
                    if let Some(id) = binder.default {
                        self.expect_kind(id, binder.kind);
                    }
                }
            }
            Type::Map { packs, pattern } => {
                assert!(!packs.is_empty(), "a mapping over no packs");
                assert!(
                    packs.len() <= usize::from(u16::MAX) + 1,
                    "binder count overflow"
                );
                for pack in packs.iter() {
                    self.expect_kind(*pack, Kind::Schema);
                }
                self.expect_kind(*pattern, Kind::Type);
            }
        }
    }

    /// Expose one transparent declaration. No substitution or scope change occurs.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn deref_one(&self, ty: TypeId) -> Option<TypeId> {
        let Type::Decl(id) = self.ty(ty) else {
            return None;
        };
        let decl = self.declaration(*id);
        if decl.source.kind.nominal() {
            return None;
        }
        Some(decl.ty)
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn expose(&self, mut ty: TypeId) -> Result<Exposure, ExposureCycle> {
        let mut declarations = Vec::new();
        let mut visited = HashSet::new();
        while let Some(next) = self.deref_one(ty) {
            let Type::Decl(id) = *self.ty(ty) else {
                unreachable!()
            };
            declarations.push(id);
            if !visited.insert(id) {
                return Err(ExposureCycle(declarations));
            }
            ty = next;
        }
        Ok(Exposure { ty, declarations })
    }

    /// Visit each `(node, enclosing-group count)` once, without exposing declarations.
    pub(crate) fn walk(&self, root: TypeId, mut visit: impl FnMut(TypeId, u32)) {
        let mut pending = vec![(root, 0u32)];
        let mut seen = HashSet::new();
        while let Some((id, depth)) = pending.pop() {
            if !seen.insert((id, depth)) {
                continue;
            }
            visit(id, depth);
            self.ty(id).visit_children(|child, groups| {
                pending.push((
                    child,
                    depth.checked_add(groups).expect("type nesting too deep"),
                ));
            });
        }
    }

    /// Insert groups at `cutoff` (positive amount), or remove groups beginning
    /// there (negative amount). References to removed groups are an error.
    /// Locally bound references beneath quantifiers are never shifted.
    pub(crate) fn shift(
        &self,
        root: TypeId,
        cutoff: u16,
        amount: i32,
    ) -> Result<TypeId, RemovedBinder> {
        self.shift_inner(root, u32::from(cutoff), amount, &mut HashMap::new())
    }

    fn shift_inner(
        &self,
        id: TypeId,
        cutoff: u32,
        amount: i32,
        memo: &mut HashMap<(TypeId, u32), TypeId>,
    ) -> Result<TypeId, RemovedBinder> {
        if let Some(result) = memo.get(&(id, cutoff)) {
            return Ok(*result);
        }
        let ty = self.ty(id);
        let mapped = if let Type::Bound { reference, kind } = *ty {
            let depth = u32::from(reference.depth);
            if depth < cutoff {
                ty.clone()
            } else {
                let shifted = i64::from(depth) + i64::from(amount);
                if shifted < i64::from(cutoff) {
                    return Err(RemovedBinder(reference));
                }
                Type::Bound {
                    reference: BoundRef {
                        depth: shifted.try_into().expect("binder depth overflow"),
                        ..reference
                    },
                    kind,
                }
            }
        } else {
            ty.map_children(|child, groups| {
                self.shift_inner(
                    child,
                    cutoff.checked_add(groups).expect("binder cutoff overflow"),
                    amount,
                    memo,
                )
            })?
        };
        let result = self.intern(mapped);
        memo.insert((id, cutoff), result);
        Ok(result)
    }

    /// Replace the references to the group a closed type is interpreted in, as a
    /// declaration's binder bounds, defaults and body are, with `args`. The result
    /// is interpreted where `args` are.
    pub(crate) fn substitute(&self, root: TypeId, args: &[TypeId]) -> TypeId {
        self.substitute_inner(root, 0, args, false, &mut HashMap::new())
    }

    /// Replace the references to the innermost group of an open type, as a
    /// mapping's pattern is, with `items`, interpreted where the type is. The
    /// result is interpreted where `items` are: the groups outside the replaced one
    /// are one nearer.
    pub(crate) fn instantiate(&self, root: TypeId, items: &[TypeId]) -> TypeId {
        self.substitute_inner(root, 0, items, true, &mut HashMap::new())
    }

    /// The variance of `root`, as a mapping's pattern is, in slot `slot` of its
    /// innermost group: what the positions referring to it have in common, or
    /// `None` if none does. A position in a form whose variance isn't known,
    /// such as a quantified type, is invariant.
    pub(crate) fn slot_variance(&self, root: TypeId, slot: u16) -> Option<Variance> {
        let mut found = None;
        self.slot_variance_inner(root, slot, 0, Variance::Covariant, &mut found);
        found
    }

    fn slot_variance_inner(
        &self,
        id: TypeId,
        slot: u16,
        local: u32,
        variance: Variance,
        found: &mut Option<Variance>,
    ) {
        let mut walk = |ty: TypeId, inner: Variance, groups: u32| {
            self.slot_variance_inner(ty, slot, local + groups, variance.compose(inner), found)
        };
        match *self.ty(id) {
            Type::Bound { reference, .. } => {
                if u32::from(reference.depth) == local && reference.slot == slot {
                    *found = match *found {
                        Some(seen) if seen != variance => Some(Variance::Invariant),
                        _ => Some(variance),
                    };
                }
            }
            Type::Apply { base, ref args, .. } => {
                let binders = match *self.ty(base) {
                    Type::Decl(decl) if self.declaration(decl).source.kind.nominal() => {
                        match self.ty(self.declaration(decl).ty) {
                            Type::Quantified { binders, .. } if binders.len() == args.len() => {
                                Some(binders)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                for (index, arg) in args.iter().enumerate() {
                    let (Argument::Positional(ty)
                    | Argument::Keyword(_, ty)
                    | Argument::Expand(ty)) = *arg;
                    let inner = match (binders, arg) {
                        (Some(binders), Argument::Positional(_)) => binders[index].variance,
                        _ => Variance::Invariant,
                    };
                    walk(ty, inner, 0);
                }
            }
            Type::Function(ref function) => {
                walk(function.params, Variance::Contravariant, 0);
                for channel in [function.input, function.output].into_iter().flatten() {
                    walk(channel, Variance::Contravariant, 0);
                }
                walk(function.result, Variance::Covariant, 0);
            }
            Type::Schema(_) => {
                self.ty(id)
                    .visit_children(|ty, groups| walk(ty, Variance::Covariant, groups));
            }
            // Each pack is where its pattern places its items, and its count
            // only adds items
            Type::Map { ref packs, pattern } => {
                for (index, &pack) in packs.iter().enumerate() {
                    let index = u16::try_from(index).expect("a mapping's packs fit a group");
                    let inner = self.slot_variance(pattern, index);
                    walk(pack, inner.unwrap_or(Variance::Covariant), 0);
                }
                walk(pattern, Variance::Covariant, 1);
            }
            Type::Union(ref members) => {
                for member in members.iter() {
                    match *member {
                        UnionMember::Type(ty) => walk(ty, Variance::Covariant, 0),
                        _ => {
                            walk(member.id(), Variance::Invariant, 0);
                            if let Some(key) = member.key() {
                                walk(key, Variance::Invariant, 0);
                            }
                        }
                    }
                }
            }
            ref ty => ty.visit_children(|ty, groups| walk(ty, Variance::Invariant, groups)),
        }
    }

    /// Replace the references to the group at `cutoff` with `args`. References
    /// beyond it are an error, unless `open` makes them one group nearer.
    fn substitute_inner(
        &self,
        id: TypeId,
        cutoff: u32,
        args: &[TypeId],
        open: bool,
        memo: &mut HashMap<(TypeId, u32), TypeId>,
    ) -> TypeId {
        if let Some(result) = memo.get(&(id, cutoff)) {
            return *result;
        }
        let ty = self.ty(id);
        let result = match *ty {
            Type::Bound { reference, kind } => {
                let depth = u32::from(reference.depth);
                if depth < cutoff {
                    id
                } else if depth > cutoff {
                    assert!(open, "reference beyond a closed type's group");
                    self.intern(Type::Bound {
                        reference: BoundRef {
                            depth: reference.depth - 1,
                            ..reference
                        },
                        kind,
                    })
                } else {
                    let arg = args[usize::from(reference.slot)];
                    self.expect_kind(arg, kind);
                    let cutoff = u16::try_from(cutoff).expect("binder depth overflow");
                    self.shift(arg, 0, i32::from(cutoff))
                        .expect("inserting groups removes none")
                }
            }
            _ => {
                let mapped = ty
                    .map_children(|child, groups| {
                        Ok::<_, std::convert::Infallible>(self.substitute_inner(
                            child,
                            cutoff.checked_add(groups).expect("binder cutoff overflow"),
                            args,
                            open,
                            memo,
                        ))
                    })
                    .unwrap_or_else(|never| match never {});
                self.intern(mapped)
            }
        };
        memo.insert((id, cutoff), result);
        result
    }

    /// Replace each fresh literal type with its class, where that is registered.
    /// A regular literal was written in a type, so it's kept, as are exact schema
    /// keys, the keys item projections select by, and binder bounds and defaults,
    /// since decaying them would change what they mean.
    pub(crate) fn decay(&self, root: TypeId) -> TypeId {
        self.decay_inner(root, &mut HashMap::new())
    }

    fn decay_inner(&self, id: TypeId, memo: &mut HashMap<TypeId, TypeId>) -> TypeId {
        if let Some(result) = memo.get(&id) {
            return *result;
        }
        let result = match self.ty(id) {
            Type::Fresh(literal) => self.intrinsic(literal.intrinsic()).unwrap_or(id),
            Type::Literal(_) => id,
            Type::Schema(items) => {
                let items = items
                    .iter()
                    .map(|item| SchemaItem {
                        multiplicity: item.multiplicity,
                        element: match item.element {
                            Element::Positional(ty) => {
                                Element::Positional(self.decay_inner(ty, memo))
                            }
                            Element::Include(ty) => Element::Include(self.decay_inner(ty, memo)),
                            Element::Keyed { key, value } => Element::Keyed {
                                key,
                                value: self.decay_inner(value, memo),
                            },
                        },
                    })
                    .collect();
                self.intern(Type::Schema(items))
            }
            Type::Quantified { binders, body } => self.intern(Type::Quantified {
                binders: binders.clone(),
                body: self.decay_inner(*body, memo),
            }),
            Type::Union(members) => {
                let members = (members.iter())
                    .map(|member| member.with(self.decay_inner(member.id(), memo)))
                    .collect();
                self.intern(Type::Union(members))
            }
            ty => {
                let mapped = ty
                    .map_children(|child, _| {
                        Ok::<_, std::convert::Infallible>(self.decay_inner(child, memo))
                    })
                    .unwrap_or_else(|never| match never {});
                self.intern(mapped)
            }
        };
        memo.insert(id, result);
        result
    }

    /// The binders of a quantified type's group that an item projection in its
    /// body selects by, by slot. Its variables keep the literals they're given,
    /// since a decayed key would select differently.
    pub(crate) fn item_keys(&self, quantified: TypeId) -> Vec<u16> {
        let Type::Quantified { body, .. } = self.ty(quantified) else {
            return Vec::new();
        };
        let mut slots = Vec::new();
        let mut pending = vec![(*body, 0u32)];
        let mut seen = HashSet::new();
        while let Some((ty, depth)) = pending.pop() {
            if !seen.insert((ty, depth)) {
                continue;
            }
            if let Type::Union(members) = self.ty(ty) {
                for key in members.iter().filter_map(|member| member.key()) {
                    if let Type::Bound { reference, .. } = *self.ty(key)
                        && u32::from(reference.depth) == depth
                        && !slots.contains(&reference.slot)
                    {
                        slots.push(reference.slot);
                    }
                }
            }
            self.ty(ty)
                .visit_children(|child, groups| pending.push((child, depth + groups)));
        }
        slots
    }

    /// Split the first `count` binders off a quantified type's group. The result is
    /// quantified over the rest, and is interpreted where a group of the first
    /// `count` binders is, so viewing it where they are bound applies them. A method,
    /// lifted over its class's binders, is applied to a class's arguments this way.
    pub(crate) fn split(&self, ty: TypeId, count: usize) -> TypeId {
        if count == 0 {
            return ty;
        }
        let Type::Quantified { binders, body } = self.ty(ty) else {
            panic!("splitting binders off a type without them")
        };
        assert!(count <= binders.len(), "splitting off too many binders");
        let rest = &binders[count..];
        let outer = usize::from(!rest.is_empty());
        let args: Vec<_> = binders
            .iter()
            .enumerate()
            .map(|(slot, binder)| {
                let reference = if slot < count {
                    BoundRef::new(outer, slot)
                } else {
                    BoundRef::new(0, slot - count)
                };
                self.intern(Type::Bound {
                    reference,
                    kind: binder.kind,
                })
            })
            .collect();
        let body = self.substitute(*body, &args);
        if rest.is_empty() {
            return body;
        }
        let binders = rest
            .iter()
            .map(|binder| Binder {
                bound: binder.bound.map(|bound| self.substitute(bound, &args)),
                default: binder
                    .default
                    .map(|default| self.substitute(default, &args)),
                ..binder.clone()
            })
            .collect();
        self.intern(Type::Quantified { binders, body })
    }

    /// Quantify a type interpreted where a group of `outer` is over that group,
    /// undoing [`Self::split`]: a type quantified over its own binders is
    /// quantified over `outer` followed by them.
    pub(crate) fn merge_groups(&self, outer: &[Binder], ty: TypeId) -> TypeId {
        if outer.is_empty() {
            return ty;
        }
        let Type::Quantified { binders, body } = self.ty(ty) else {
            return self.intern(Type::Quantified {
                binders: outer.iter().cloned().collect(),
                body: ty,
            });
        };
        // The inner group's bounds, defaults and body see it at depth 0 and the
        // outer group at depth 1
        let count = outer.len();
        let mut memo = HashMap::new();
        let mut merge = |ty| self.merge_inner(ty, 0, count, &mut memo);
        let inner: Vec<_> = binders
            .iter()
            .map(|binder| Binder {
                bound: binder.bound.map(&mut merge),
                default: binder.default.map(&mut merge),
                ..binder.clone()
            })
            .collect();
        let body = merge(*body);
        self.intern(Type::Quantified {
            binders: outer.iter().cloned().chain(inner).collect(),
            body,
        })
    }

    fn merge_inner(
        &self,
        id: TypeId,
        cutoff: u32,
        count: usize,
        memo: &mut HashMap<(TypeId, u32), TypeId>,
    ) -> TypeId {
        if let Some(result) = memo.get(&(id, cutoff)) {
            return *result;
        }
        let ty = self.ty(id);
        let mapped = match *ty {
            Type::Bound { reference, kind } => {
                let depth = u32::from(reference.depth);
                let reference = if depth < cutoff {
                    reference
                } else if depth == cutoff {
                    BoundRef {
                        slot: reference.slot + u16::try_from(count).expect("binder slot overflow"),
                        ..reference
                    }
                } else {
                    BoundRef {
                        depth: reference.depth - 1,
                        ..reference
                    }
                };
                Type::Bound { reference, kind }
            }
            _ => ty
                .map_children(|child, groups| {
                    Ok::<_, std::convert::Infallible>(self.merge_inner(
                        child,
                        cutoff.checked_add(groups).expect("binder cutoff overflow"),
                        count,
                        memo,
                    ))
                })
                .unwrap_or_else(|never| match never {}),
        };
        let result = self.intern(mapped);
        memo.insert((id, cutoff), result);
        result
    }

    /// The shape of a rest mode: `{*Value}`, `{**Sym: Value}` or both. The key is
    /// `Unknown` when `Sym` is not designated.
    pub(crate) fn rest_shape(&self, rest: Rest) -> TypeId {
        let top = self.top();
        let key = self.intrinsic(Intrinsic::Sym).unwrap_or(self.unknown());
        let positional = SchemaItem {
            multiplicity: Multiplicity::Repeated,
            element: Element::Positional(top),
        };
        let keyed = SchemaItem {
            multiplicity: Multiplicity::Repeated,
            element: Element::Keyed { key, value: top },
        };
        let items = match rest {
            Rest::Positional => vec![positional],
            Rest::Keyed => vec![keyed],
            Rest::All => vec![positional, keyed],
        };
        self.intern(Type::Schema(items.into()))
    }

    /// A binder's bound with `args` for its group, or its rest mode's shape for an
    /// unbounded rest binder
    pub(crate) fn binder_bound(&self, binder: &Binder, args: &[TypeId]) -> Option<TypeId> {
        match (binder.bound, binder.binding) {
            (Some(bound), _) => Some(self.substitute(bound, args)),
            (None, Binding::Rest(rest)) => Some(self.rest_shape(rest)),
            (None, _) => None,
        }
    }

    /// A class applied to `given` for its first binders and to its later binders'
    /// defaults, each of which sees the arguments before it; `None` if one of
    /// those has no default
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn apply_defaults(&self, decl: DeclId, given: &[TypeId]) -> Option<TypeId> {
        let Type::Quantified { binders, .. } = self.ty(self.declaration(decl).ty) else {
            return None;
        };
        let mut args: Vec<TypeId> = binders.iter().map(|b| self.unknown_of(b.kind)).collect();
        args[..given.len()].copy_from_slice(given);
        for slot in given.len()..binders.len() {
            args[slot] = self.substitute(binders[slot].default?, &args);
        }
        Some(self.intern(Type::Apply {
            base: self.intern(Type::Decl(decl)),
            args: args.into_iter().map(Argument::Positional).collect(),
            kind: Kind::Type,
        }))
    }

    /// The rigids of a declaration's binders, in slot order. Substituting them for
    /// its group gives the declaration as checked.
    pub(crate) fn rigids(&self, decl: DeclId) -> Vec<TypeId> {
        let Type::Quantified { binders, .. } = self.ty(self.declaration(decl).ty) else {
            return Vec::new();
        };
        binders
            .iter()
            .enumerate()
            .map(|(slot, binder)| {
                self.intern(Type::Rigid {
                    decl,
                    slot: slot.try_into().expect("binder slot overflow"),
                    kind: binder.kind,
                })
            })
            .collect()
    }

    /// Replace `decl`'s rigids with references to its group, undoing [`Self::substitute`]
    /// with [`Self::rigids`]. Another declaration's rigid has escaped its check.
    pub(crate) fn abstract_rigids(&self, root: TypeId, decl: DeclId) -> Result<TypeId, Escape> {
        self.abstract_inner(root, 0, decl, &mut HashMap::new())
    }

    fn abstract_inner(
        &self,
        id: TypeId,
        cutoff: u32,
        decl: DeclId,
        memo: &mut HashMap<(TypeId, u32), TypeId>,
    ) -> Result<TypeId, Escape> {
        if let Some(result) = memo.get(&(id, cutoff)) {
            return Ok(*result);
        }
        let ty = self.ty(id);
        let result = match *ty {
            Type::Rigid {
                decl: owner,
                slot,
                kind,
            } => {
                if owner != decl {
                    return Err(Escape(id));
                }
                self.intern(Type::Bound {
                    reference: BoundRef {
                        depth: cutoff.try_into().expect("binder depth overflow"),
                        slot,
                    },
                    kind,
                })
            }
            _ => self.intern(ty.map_children(|child, groups| {
                self.abstract_inner(
                    child,
                    cutoff.checked_add(groups).expect("binder cutoff overflow"),
                    decl,
                    memo,
                )
            })?),
        };
        memo.insert((id, cutoff), result);
        Ok(result)
    }
}

mod render;
#[cfg(test)]
mod tests;

pub(crate) use render::{Names, Style};
