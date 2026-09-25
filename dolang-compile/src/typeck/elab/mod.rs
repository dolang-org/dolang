//! Elaboration of the checked units' declarations into the type database.
//!
//! Passes run in order over common tables and the units' frozen syntax trees. The
//! tables refer to declaration nodes in place rather than copying source into an
//! intermediate representation.

mod capture;
mod collect;
mod judge;
mod kind;
mod populate;
mod sig;
mod variance;

use std::{
    collections::HashMap,
    fmt::{self, Write},
};

use super::r#type::{DeclId, DeclKind, Intrinsic, Kind, TypeId, UnitId, UnitSpan, Variance};
use crate::{
    Compiler, RestKind, Unit,
    ast::{Binder, Class, Def, Function, Method, Param, TypeAlias, TypeExpr, visit::Node},
    diag::{AnnotationKind, NoteKind, Severity},
    source::{Annotate, Diagnose, Note, Span},
};

pub(crate) use capture::captures;
pub(crate) use collect::{UnitDiag, collect};
pub(crate) use judge::JUDGMENTS;
pub(crate) use kind::{Fill, kinds};
pub(crate) use populate::populate;
pub(crate) use sig::signatures;
pub(crate) use variance::variances;

/// What collection learns of the checked units
pub(crate) struct Tables<'u> {
    /// The units, by [`UnitId`]
    pub(crate) units: Vec<&'u Unit<'u>>,
    /// Every declaration, by [`DeclId`]
    pub(crate) decls: Vec<Decl<'u>>,
    /// What each type name refers to, keyed by its head. Imports and renames are
    /// chased away; aliases are not, so `Pair[Int]` refers to the `Pair` alias.
    pub(crate) referents: HashMap<UnitSpan, Referent>,
    /// The underlying head of each transparent alias
    pub(crate) aliases: HashMap<DeclId, Head>,
    /// Each unit's exports by name, with the name each is bound by. Empty for a unit
    /// that is not a module.
    pub(crate) exports: Vec<HashMap<&'u str, (Span, Target<'u>)>>,
    /// Every type expression written in a declaration or annotation, outermost only
    pub(crate) sites: Vec<Site<'u>>,
    /// The kind of each binder, including the implicit binders of omitted ambient
    /// channels
    pub(crate) binder_kinds: HashMap<BinderRef, KindOf>,
    /// The kind of each alias
    pub(crate) alias_kinds: HashMap<DeclId, KindOf>,
    /// The completed signature of each def or method, by declaration and signature
    pub(crate) sigs: HashMap<(DeclId, usize), Sig<'u>>,
    /// The type of each field, by its class and the span of its name
    pub(crate) fields: HashMap<(DeclId, Span), Slot<'u>>,
    /// The ambient channels of each function type written without them, by its `->`
    pub(crate) func_ambients: HashMap<UnitSpan, [Ambient; 2]>,
    /// The declarations of `std` the checker treats specially
    pub(crate) designated: HashMap<DeclId, Designated>,
    /// The variance of each binder, including the implicit binders of omitted ambient
    /// channels
    pub(crate) variance: HashMap<BinderRef, Variance>,
    /// The variance of each binder of an enclosing declaration that a nested one uses.
    /// A binder it does not use is absent, and invariant if it is captured anyway.
    pub(crate) captured: HashMap<(DeclId, BinderRef), Variance>,
    /// The binders of enclosing declarations each declaration is lifted over, which
    /// lead its binder group, outermost first
    pub(crate) lifted: HashMap<DeclId, Vec<BinderRef>>,
    /// The database declaration of each def or method signature. A function's own
    /// ID holds its implementation, or its first signature when it has none.
    pub(crate) sig_decls: HashMap<(DeclId, usize), DeclId>,
    /// The binder group of each declaration signature: the binders it is lifted
    /// over, its written binders, then its implicit binders
    pub(crate) groups: HashMap<(DeclId, usize), Vec<BinderRef>>,
    /// Each type expression written in source, interned in its group, by its span
    pub(crate) site_types: HashMap<UnitSpan, TypeId>,
}

impl<'u> Tables<'u> {
    /// The number of signatures of a declaration: its defs or methods, or 1
    pub(crate) fn sig_count(&self, decl: DeclId) -> usize {
        match &self.decls[decl.index()].node {
            DeclNode::Defs(defs) => defs.len(),
            DeclNode::Methods(methods) => methods.len(),
            DeclNode::Class(_) | DeclNode::Alias(_) | DeclNode::Closure(_) => 1,
        }
    }

    /// The binders written for signature `sig` of a declaration
    pub(crate) fn binders(&self, decl: DeclId, sig: usize) -> &'u [Binder] {
        let binders = match self.decls[decl.index()].node {
            DeclNode::Class(class) => class.binders.as_deref(),
            DeclNode::Alias(alias) => alias.binders.as_deref(),
            DeclNode::Defs(ref defs) => defs[sig].binders.as_deref(),
            DeclNode::Methods(ref methods) => methods[sig].binders.as_deref(),
            DeclNode::Closure(_) => None,
        };
        binders.map_or(&[], |binders| &binders.binders)
    }

    /// Each written type as interned, with the kinds of the binders of the group it
    /// is interpreted in
    pub(crate) fn site_kinds(&self) -> impl Iterator<Item = (TypeId, Vec<Kind>)> + '_ {
        self.sites.iter().map(|site| {
            let ty = self.site_types[&UnitSpan {
                unit: site.unit,
                span: site.ty.span(),
            }];
            let kinds = site.group().map_or_else(Vec::new, |key| {
                self.groups[&key]
                    .iter()
                    .map(|binder| self.binder_kinds[binder].kind)
                    .collect()
            });
            (ty, kinds)
        })
    }

    /// The source text of a span of a unit
    pub(crate) fn text(&self, unit: UnitId, span: Span) -> &'u str {
        self.units[unit.index()].compiler.file.str(span)
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
pub(crate) struct Site<'u> {
    pub(crate) unit: UnitId,
    pub(crate) ty: &'u TypeExpr,
    pub(crate) role: Role,
    /// The def or method signature whose ambient channels a function type written
    /// here takes when it omits its own
    pub(crate) ambient: Option<(DeclId, usize)>,
    /// The declaration signature whose body or signature the type is written in,
    /// absent at a unit's top level
    pub(crate) owner: Option<(DeclId, usize)>,
}

impl Site<'_> {
    /// The declaration signature whose binder group the type is interpreted in
    pub(crate) fn group(&self) -> Option<(DeclId, usize)> {
        match self.role {
            Role::Bound(binder) | Role::Default(binder) => Some((binder.decl, binder.sig)),
            Role::Alias(decl) => Some((decl, 0)),
            Role::Type | Role::Rest | Role::Pattern => self.owner,
        }
    }
}

#[derive(Clone, Copy, Debug)]
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
pub(crate) enum Slot<'u> {
    Annot(&'u TypeExpr),
    /// Omitted, so dynamic
    Unknown,
    /// An omitted receiver annotation: the class applied to its own binders
    SelfType,
}

/// The type of a rest parameter, before it is interned
#[derive(Clone, Copy)]
pub(crate) enum RestSlot<'u> {
    /// A type for each item: `{*T}`, `{**T}` or `{*T, **T}` by the rest's kind
    Items(RestKind, Slot<'u>),
    /// A schema for the whole pack
    Pack(&'u TypeExpr),
    /// A type pattern expanded over the packs it names
    Pattern(&'u TypeExpr),
}

#[derive(Clone, Copy)]
pub(crate) enum ParamTy<'u> {
    Single(Slot<'u>),
    Rest(RestSlot<'u>),
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
    /// Dynamic, outside any def
    Unknown,
}

/// A def or method signature, completed with the defaults for what it omits
pub(crate) struct Sig<'u> {
    pub(crate) params: Vec<(&'u Param, ParamTy<'u>)>,
    /// Whether the first parameter is an instance method's receiver
    pub(crate) receiver: bool,
    pub(crate) input: Ambient,
    pub(crate) output: Ambient,
    pub(crate) ret: Slot<'u>,
}

/// A declaration of `std` the checker treats specially
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Designated {
    /// `std.Value`, which is top
    Value,
    /// `std.Phantom`, which marks its arguments as used covariantly
    Phantom,
    Intrinsic(Intrinsic),
}

/// A source declaration
pub(crate) struct Decl<'u> {
    pub(crate) unit: UnitId,
    pub(crate) kind: DeclKind,
    /// The declared name; absent for a closure
    pub(crate) name: Option<Span>,
    pub(crate) node: DeclNode<'u>,
    /// The declaration this one is nested in, and which of its signatures, whose
    /// binders it may capture
    pub(crate) outer: Option<(DeclId, usize)>,
}

pub(crate) enum DeclNode<'u> {
    /// A class or protocol
    Class(&'u Class),
    /// A transparent or opaque alias
    Alias(&'u TypeAlias),
    /// A function's implementation and its `@def` overloads, in source order. The
    /// implementation is absent when only overloads were written.
    Defs(Vec<&'u Def>),
    /// The methods of one name in a class body, in source order
    Methods(Vec<&'u Method>),
    /// A lambda or field initializer
    Closure(&'u Function),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Referent {
    Decl(DeclId),
    Binder(BinderRef),
    /// An item of a module that no checked unit provides
    External {
        module: Box<str>,
        item: Box<str>,
    },
    /// A module, reached by a name that is not dotted
    Module(ModuleRef),
    /// A binding that exists only at runtime, such as a `let` or a parameter
    Value(UnitSpan),
    /// Nothing, for a reason already diagnosed
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModuleRef {
    Unit(UnitId),
    External(Box<str>),
}

/// What an unresolved name or export refers to
#[derive(Clone, Debug)]
pub(crate) enum Target<'u> {
    Local(Referent),
    /// An item of a module, by name
    Import {
        module: &'u str,
        item: &'u str,
    },
    /// A module, by name
    Module(&'u str),
}

/// The head of a transparent alias's underlying type
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Head {
    /// A class, protocol or opaque alias, or a declaration that is not a type
    Decl(DeclId),
    Binder(BinderRef),
    External {
        module: Box<str>,
        item: Box<str>,
    },
    /// A union, function type, schema or constant
    Structural,
    /// Nothing, for a reason already diagnosed
    Error,
}

struct ImportCycle {
    span: Span,
    chain: String,
}

impl Diagnose for ImportCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "import cycle")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn notes(&self) -> Box<dyn Iterator<Item = Box<dyn Note>>> {
        Box::new(std::iter::once(
            Box::new(Chain(self.chain.clone())) as Box<dyn Note>
        ))
    }
}

struct Chain(String);

impl Note for Chain {
    fn kind(&self) -> NoteKind {
        NoteKind::Info
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "{} re-export each other", self.0)
    }
}

struct AliasCycle(Span);

impl Diagnose for AliasCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "alias refers to itself")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct MissingExport {
    span: Span,
    module: String,
    item: String,
}

impl Diagnose for MissingExport {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

impl Diagnose for NotAType {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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
}

impl Diagnose for KindMismatch {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        match self.declared {
            Some(_) => Box::new(std::iter::once(Box::new(self.clone()) as Box<dyn Annotate>)),
            None => Box::new(std::iter::empty()),
        }
    }
}

impl Annotate for KindMismatch {
    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }

    fn span(&self) -> Span {
        self.declared.expect("annotated only when declared")
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "declared here")
    }
}

/// Type arguments applied to what takes none
struct NotGeneric {
    span: Span,
    /// Whether the base is a schema rather than a non-generic type
    schema: bool,
}

impl Diagnose for NotGeneric {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

impl Diagnose for TooManyTypeArgs {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

impl Diagnose for UnknownTypeKeyword {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

/// A rest binding's `@...` pattern that names no pack
struct PatternWithoutPack(Span);

impl Diagnose for PatternWithoutPack {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
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

impl Diagnose for MisdeclaredIntrinsic {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "the checker requires this to be {}", self.expected)
    }

    fn span(&self) -> Span {
        self.span
    }
}
