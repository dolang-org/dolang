//! Elaboration of the checked units' declarations into the type database.
//!
//! Collection copies each unit's declaration surface out of its syntax tree (see
//! [`surface`]), and the passes after it read only that and the common tables. Only
//! lowering reads syntax trees again, for the bodies it lowers.

mod capture;
mod collect;
mod ids;
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
    collections::HashMap,
    fmt::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use super::report::{Annotation, Report};
use super::r#type::{
    Database, DeclId, DeclKind, Intrinsic, Kind, TypeId, UnitId, UnitSpan, Variance,
};
use super::typelib::wire;
use crate::{
    RestKind, Unit, ast,
    diag::{AnnotationKind, NoteKind, Severity},
    source::Span,
};
use surface::{Alias, Binder, Class, Closure, Def, Method, Name, Signature, SiteId, TypeExpr};

pub(crate) use super::report::{Diag, UnitDiag};
pub(crate) use capture::captures;
pub(crate) use collect::{Harvest, Pending, harvest, link};
pub(crate) use ids::Ids;
pub(crate) use judge::JUDGMENTS;
pub(crate) use kind::{Fill, kinds};
pub(crate) use overrides::overrides;
pub(crate) use populate::populate;
pub(crate) use sig::signatures;
pub(crate) use specialize::specialize;
pub(crate) use variance::variances;
pub(crate) use wellformed::{Unresolved, wellformed};

/// The names of `strand`'s pipe placeholders, sender first
pub(crate) const PIPES: [&str; 2] = ["PipeSender", "PipeReceiver"];

/// What collection learns of the checked units
pub(crate) struct Tables<'u> {
    /// The units, by [`UnitId`]
    pub(crate) units: Vec<UnitInfo<'u>>,
    /// Each unit's string table, by [`UnitId`]
    pub(crate) strings: Vec<Vec<&'u str>>,
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
    /// Every type expression written in a declaration or annotation, outermost only,
    /// by [`SiteId`]
    pub(crate) sites: Vec<Site>,
    /// The kind of each binder, including the implicit binders of omitted ambient
    /// channels
    pub(crate) binder_kinds: HashMap<BinderRef, KindOf>,
    /// The kind of each alias
    pub(crate) alias_kinds: HashMap<DeclId, KindOf>,
    /// The completed signature of each def or method, by declaration and signature
    pub(crate) sigs: HashMap<(DeclId, usize), Sig>,
    /// The type of each field, by its class and the span of its name
    pub(crate) fields: HashMap<(DeclId, Span), Slot>,
    /// The ambient channels of each function type written without them, by its `->`
    pub(crate) func_ambients: HashMap<UnitSpan, [Ambient; 2]>,
    /// The declarations of `std` and `strand` the checker treats specially
    pub(crate) designated: HashMap<DeclId, Designated>,
    /// What each of `strand`'s pipe placeholders nominates, by name, resolved where
    /// the placeholder is exported
    pub(crate) nominees: HashMap<&'static str, Referent>,
    /// The type each designated pipe placeholder stands for, or `None` for a
    /// nominee that isn't checked or can't stand for it
    pub(crate) pipes: HashMap<DeclId, Option<DeclId>>,
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
    /// Each application and function type written in source, nested or not,
    /// interned in its group, by its span
    pub(crate) expr_types: HashMap<UnitSpan, TypeId>,
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
    pub(crate) fn binders(&self, decl: DeclId, sig: usize) -> &[Binder] {
        match &self.decls[decl.index()].node {
            DeclNode::Class(class) => &class.binders,
            DeclNode::Alias(alias) => &alias.binders,
            DeclNode::Defs(defs) => &defs[sig].binders,
            DeclNode::Methods(methods) => &methods[sig].binders,
            DeclNode::Closure(_) => &[],
        }
    }

    /// Signature `sig` of a method declaration
    pub(crate) fn method(&self, decl: DeclId, sig: usize) -> &Method {
        match &self.decls[decl.index()].node {
            DeclNode::Methods(methods) => &methods[sig],
            _ => unreachable!("only a method declaration has methods"),
        }
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

    /// The rigids a declaration signature's body is checked under, one for each
    /// binder of its group. A binder lifted from an enclosing declaration is that
    /// declaration's rigid, so a closure's types agree with its enclosing def's.
    pub(crate) fn group_rigids(&self, db: &Database, key: (DeclId, usize)) -> Vec<TypeId> {
        let Some(group) = self.groups.get(&key) else {
            return Vec::new();
        };
        group
            .iter()
            .map(|binder| {
                let owner = (binder.decl, binder.sig);
                let slot = self.groups[&owner]
                    .iter()
                    .position(|other| other == binder)
                    .expect("a binder is in its own declaration's group");
                let rigids = db.rigids(self.sig_decls[&owner]);
                // A group too large to populate left its declaration without binders
                rigids
                    .get(slot)
                    .copied()
                    .unwrap_or_else(|| db.unknown_of(self.binder_kinds[binder].kind))
            })
            .collect()
    }

    /// The signature whose database declaration a declaration's own ID holds: its
    /// implementation, or its first signature when it has none
    pub(crate) fn primary_sig(&self, decl: DeclId) -> usize {
        (0..self.sig_count(decl))
            .find(|&sig| self.sig_decls.get(&(decl, sig)) == Some(&decl))
            .unwrap_or(0)
    }

    /// The source text of a span of a unit, for lowering and flow analysis, which
    /// read only units with source
    pub(crate) fn text(&self, unit: UnitId, span: Span) -> &'u str {
        self.units[unit.index()]
            .source
            .expect("only a unit with source is read as text")
            .compiler
            .file
            .str(span)
    }
}

/// The unit of a harvest that is not yet linked
pub(crate) fn local() -> UnitId {
    UnitId::from_index(0)
}

/// What the checker knows of a unit, whether or not its source is at hand
#[derive(Clone)]
pub(crate) struct UnitInfo<'u> {
    /// The module's name; absent for a script
    pub(crate) module: Option<&'u str>,
    pub(crate) path: &'u Path,
    /// The offset of each newline of the unit's source, which locates its spans
    pub(crate) newlines: Vec<u32>,
    /// The unit, when it is checked from source
    pub(crate) source: Option<&'u Unit<'u>>,
}

impl UnitInfo<'_> {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct BinderRef {
    pub(crate) decl: DeclId,
    pub(crate) sig: usize,
    pub(crate) slot: usize,
}

/// A type expression written in source, and how it is used
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Site {
    /// A harvest's own unit, which a typelib leaves implicit
    #[serde(skip, default = "local")]
    pub(crate) unit: UnitId,
    pub(crate) ty: TypeExpr,
    pub(crate) role: Role,
    /// The def or method signature whose ambient channels a function type written
    /// here takes when it omits its own
    pub(crate) ambient: Option<(DeclId, usize)>,
    /// The declaration signature whose body or signature the type is written in,
    /// absent at a unit's top level
    pub(crate) owner: Option<(DeclId, usize)>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Dynamic, outside any def
    Unknown,
}

/// A def or method signature, completed with the defaults for what it omits
pub(crate) struct Sig {
    /// Each parameter's type, by its index in the signature's parameters
    pub(crate) params: Vec<ParamTy>,
    /// Whether the first parameter is an instance method's receiver
    pub(crate) receiver: bool,
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
    /// `std.Phantom`, which marks its arguments as used covariantly
    Phantom,
    /// `std.getter`, which makes a method a computed field's getter
    Getter,
    /// `std.setter`, which makes a method a computed field's setter
    Setter,
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
    /// `std.BaseArray`, the read-only half of `std.Array` that an array literal
    /// may be expected to be
    BaseArray,
    /// `std.Dict`, the class of a dict literal
    Dict,
    /// `std.BaseDict`, the read-only half of `std.Dict` that a dict literal may be
    /// expected to be
    BaseDict,
    /// `std.Record`, the class of a record literal
    Record,
    /// `std.Range`, the class of a range
    Range,
    /// `std.BaseIterable`, which a `for` iterates
    BaseIterable,
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
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Decl<'u> {
    /// A harvest's own unit, which a typelib leaves implicit
    #[serde(skip, default = "local")]
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
    #[serde(skip)]
    pub(crate) ast: Option<DeclAst<'u>>,
}

/// A declaration's surface
#[derive(Clone, Serialize, Deserialize)]
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
    Alias(&'u ast::TypeAlias),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    Value(#[serde(with = "wire::local_span")] UnitSpan),
    /// Nothing, for a reason already diagnosed
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ModuleRef {
    /// A checked unit, which a harvest names only once linked
    #[serde(skip)]
    Unit(UnitId),
    External(Box<str>),
}

/// What an unresolved name or export refers to
#[derive(Clone, Debug, Serialize, Deserialize)]
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

/// An ambient channel annotation that isn't an `Iter` or a `Sink`
struct BadChannel {
    span: Span,
    output: bool,
}

impl Report for BadChannel {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.output {
            false => write!(w, "`<` must be an `Iter`"),
            true => write!(w, "`>` must be a `Sink`"),
        }
    }

    fn span(&self) -> Span {
        self.span
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
