//! Canonical structures and allocated source declarations.
//!
//! IDs belong to one database; they must not be mixed between databases. Equality
//! is structural, not a subtype judgment. In particular, equal open types can
//! mean different things in different environments. Declaration references retain
//! source identity and are leaves of structural traversal.
//!
//! Each nonempty quantifier introduces one group. Its entire group is in scope
//! in its bounds, defaults, and body. A bound reference counts groups outward,
//! then selects a slot in declaration order. Declarations' binder metadata is
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
    cell::RefCell,
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use dolang_util::{alias, intern};

use crate::source::Span;

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Variance {
    Invariant,
    Covariant,
    Contravariant,
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
    Expand(TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Type {
    Top,
    /// The dynamic type or schema, consistent with every type or schema of its kind
    Unknown(Kind),
    Literal(Literal),
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
}

impl Type {
    /// Visit immediate children without rebuilding the node. `groups` counts
    /// quantifier boundaries crossed, including those around bounds/defaults.
    pub(crate) fn visit_children(&self, mut visit: impl FnMut(TypeId, u32)) {
        match self {
            Self::Top
            | Self::Unknown(_)
            | Self::Literal(_)
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
                    let (UnionMember::Type(ty) | UnionMember::Expand(ty)) = member;
                    visit(*ty, 0);
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
            | Self::Literal(_)
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
                    let (UnionMember::Type(ty) | UnionMember::Expand(ty)) = member;
                    *ty = f(*ty, 0)?;
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
/// parentheses, apart from an ordinary member of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MemberKey {
    pub(crate) name: SymbolId,
    pub(crate) special: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum Member {
    /// Its type is interpreted in the scope of the class's outer binder group.
    Field {
        ty: TypeId,
        scope: Scope,
        public: bool,
    },
    /// A function declaration, lifted over all of the class's binders
    Method {
        decl: DeclId,
        scope: Scope,
        public: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
pub(crate) struct Declaration {
    pub(crate) source: DeclSource,
    pub(crate) ty: TypeId,
    pub(crate) binders: alias::Box<[BinderSource]>,
    /// Interpreted in the scope of `ty`'s outer binder group, when present.
    pub(crate) supertypes: alias::Box<[TypeId]>,
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
pub(crate) struct ExposureCycle(pub(crate) Vec<DeclId>);

/// A reference that prevents removing its enclosing binder group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemovedBinder(pub(crate) BoundRef);

/// A rigid of a declaration other than the one being abstracted over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Escape(pub(crate) TypeId);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Exposure {
    pub(crate) ty: TypeId,
    /// Transparent wrappers traversed, in source-to-underlying order.
    pub(crate) declarations: Vec<DeclId>,
}

/// Stub types recognized by elaboration and intrinsic subtype rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intrinsic {
    Union,
    /// Nominal supertype of structural function types; generic semantics are deferred.
    Func,
    Int,
    Bool,
    Sym,
    Nil,
    Str,
    /// Bounds an omitted ambient input channel, as `Iter[Unknown]`
    Iter,
    /// Bounds an omitted ambient output channel, as `Sink[Unknown]`
    Sink,
}

/// Optional associations to elaborated stub types, populated before sealing.
#[derive(Default)]
struct Intrinsics {
    union: Option<TypeId>,
    func: Option<TypeId>,
    int: Option<TypeId>,
    bool: Option<TypeId>,
    sym: Option<TypeId>,
    nil: Option<TypeId>,
    str: Option<TypeId>,
    iter: Option<TypeId>,
    sink: Option<TypeId>,
}

impl Intrinsics {
    fn get(&self, intrinsic: Intrinsic) -> Option<TypeId> {
        match intrinsic {
            Intrinsic::Union => self.union,
            Intrinsic::Func => self.func,
            Intrinsic::Int => self.int,
            Intrinsic::Bool => self.bool,
            Intrinsic::Sym => self.sym,
            Intrinsic::Nil => self.nil,
            Intrinsic::Str => self.str,
            Intrinsic::Iter => self.iter,
            Intrinsic::Sink => self.sink,
        }
    }

    fn slot_mut(&mut self, intrinsic: Intrinsic) -> &mut Option<TypeId> {
        match intrinsic {
            Intrinsic::Union => &mut self.union,
            Intrinsic::Func => &mut self.func,
            Intrinsic::Int => &mut self.int,
            Intrinsic::Bool => &mut self.bool,
            Intrinsic::Sym => &mut self.sym,
            Intrinsic::Nil => &mut self.nil,
            Intrinsic::Str => &mut self.str,
            Intrinsic::Iter => &mut self.iter,
            Intrinsic::Sink => &mut self.sink,
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

    /// The dynamic type or schema of a kind
    pub(crate) fn unknown_of(&self, kind: Kind) -> TypeId {
        match kind {
            Kind::Type => self.unknown,
            Kind::Schema => self.unknown_schema,
        }
    }

    /// The signatures of an overloaded function, in source order, including its
    /// own. Empty for a function with one signature.
    pub(crate) fn overloads(&self, id: DeclId) -> &[DeclId] {
        self.overloads.get(&id).map_or(&[], |overloads| overloads)
    }

    /// Record the signatures of an overloaded function once, before sealing.
    pub(crate) fn set_overloads(&mut self, id: DeclId, overloads: Vec<DeclId>) {
        self.require_open();
        assert!(overloads.contains(&id), "overloads omit their function");
        assert!(
            self.overloads.insert(id, overloads.into()).is_none(),
            "overloads already set: {id:?}"
        );
    }

    pub(crate) fn intrinsic(&self, intrinsic: Intrinsic) -> Option<TypeId> {
        self.intrinsics.get(intrinsic)
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
        for &supertype in declaration.supertypes.iter() {
            self.expect_kind(supertype, Kind::Type);
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
                Member::Method { .. } => None,
            });
        for root in [declaration.ty]
            .into_iter()
            .chain(declaration.supertypes.iter().copied())
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
            for (_, member) in declaration.members.iter() {
                if let Member::Method { decl, .. } = member {
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
            Type::Schema(_) => Some(Kind::Schema),
            Type::Bound { kind, .. } | Type::Rigid { kind, .. } | Type::Apply { kind, .. } => {
                Some(*kind)
            }
            Type::Decl(id) => self
                .declarations
                .get(*id)
                .map(|decl| decl.source.result_kind),
            Type::Quantified { body, .. } => self.known_kind(*body),
            Type::Unknown(kind) => Some(*kind),
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
        let ty = match ty {
            Type::Quantified { binders, body } if binders.is_empty() => return body,
            Type::Union(members) => {
                let mut normalized = Vec::new();
                let mut top = None;
                for member in members.iter().copied() {
                    match member {
                        UnionMember::Type(id) => match self.ty(id) {
                            Type::Top => top = Some(id),
                            Type::Union(nested) => normalized.extend_from_slice(nested),
                            _ => normalized.push(member),
                        },
                        UnionMember::Expand(_) => normalized.push(member),
                    }
                }
                if let Some(top) = top {
                    return top;
                }
                normalized.sort_unstable();
                normalized.dedup();
                match normalized.as_slice() {
                    [UnionMember::Type(id)] => return *id,
                    _ => Type::Union(normalized.into()),
                }
            }
            ty => ty,
        };
        self.types.id_owned(ty)
    }

    fn validate(&self, ty: &Type) {
        match ty {
            Type::Top | Type::Unknown(_) | Type::Literal(_) | Type::Bound { .. } => {}
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
                        UnionMember::Expand(id) => self.expect_kind(*id, Kind::Schema),
                    }
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
        }
    }

    /// Expose one transparent declaration. No substitution or scope change occurs.
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
        self.substitute_inner(root, 0, args, &mut HashMap::new())
    }

    fn substitute_inner(
        &self,
        id: TypeId,
        cutoff: u32,
        args: &[TypeId],
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
                } else {
                    assert_eq!(depth, cutoff, "reference beyond a closed type's group");
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

#[cfg(test)]
mod tests;
