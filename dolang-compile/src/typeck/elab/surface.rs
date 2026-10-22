//! The declaration surface: what elaboration reads of the checked units, copied out
//! of their syntax trees by collection.
//!
//! Nothing here refers to a syntax tree or to source text. Spans are kept for
//! diagnostics, a name refers through its link to what linking resolved it to, and
//! what is declared is spelled through each unit's string table, so a unit's surface
//! means the same thing whether or not its source is at hand. A typelib omits every
//! span but those that locate a definition or an import.
//!
//! What a later pass finds of a written form, such as the type it is interned as,
//! is kept in a slot on the form. Slots are relaxed atomics, filled before the
//! tables are shared, and are not serialized.

use std::{
    fmt::{self, Write},
    sync::atomic::{AtomicU8, Ordering},
};

use dolang_util::intern::AtomicId;
use serde::{Deserialize, Serialize};

use super::{Designated, Referent, Tables};
use crate::{
    RestKind,
    ast::SpecialMethod,
    source::Span,
    typeck::{
        r#type::{DeclId, Intrinsic, TypeTag, UnitId},
        typelib::wire,
    },
};

/// A string of a unit's string table
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct StrId(u32);

impl StrId {
    pub(crate) fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("a unit's string table fits in a u32"))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// A site in [`Tables::sites`]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct SiteId(u32);

impl SiteId {
    pub(crate) fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("the sites fit in a u32"))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// An entry of [`Tables::referents`]: something a unit's names refer to, as
/// collection resolved it
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LinkId(u32);

impl LinkId {
    pub(crate) fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("the links fit in a u32"))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// A module of a unit's module table
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ModuleId(u32);

impl ModuleId {
    pub(crate) fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("a unit's module table fits in a u32"))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// The type a written form was interned as, filled by population
pub(crate) type Interned = AtomicId<TypeTag>;

/// The collection a schema is where a type is required, filled by kind checking
#[derive(Default)]
pub(crate) struct Collection(AtomicU8);

impl Collection {
    const DICT: u8 = 1;
    const TUPLE: u8 = 2;

    /// `std.Dict`, or `std.Tuple`, or `None` where a schema is one
    pub(crate) fn get(&self) -> Option<Designated> {
        match self.0.load(Ordering::Relaxed) {
            Self::DICT => Some(Designated::Dict),
            Self::TUPLE => Some(Designated::Intrinsic(Intrinsic::Tuple)),
            _ => None,
        }
    }

    pub(crate) fn set(&self, tuple: bool) {
        let class = match tuple {
            true => Self::TUPLE,
            false => Self::DICT,
        };
        self.0.store(class, Ordering::Relaxed);
    }
}

impl Clone for Collection {
    fn clone(&self) -> Self {
        Self(AtomicU8::new(self.0.load(Ordering::Relaxed)))
    }
}

/// A name as written, and where
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Name {
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
    pub(crate) text: StrId,
}

/// A possibly dotted name written to refer to something. A typelib keeps only what
/// it refers to, not how it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ref {
    /// The name with any fields dotted onto it
    #[serde(skip, default = "wire::invalid")]
    pub(crate) span: Span,
    /// What it refers to, with its fields; absent where collection resolved nothing
    pub(crate) link: Option<LinkId>,
}

/// A symbol a type is written with, such as a key or a symbol constant, and where.
/// A typelib omits the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Symbol {
    #[serde(skip, default = "wire::invalid")]
    pub(crate) span: Span,
    pub(crate) text: StrId,
}

impl From<Name> for Symbol {
    fn from(name: Name) -> Self {
        Self {
            span: name.span,
            text: name.text,
        }
    }
}

/// A type expression
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeExpr {
    /// A possibly dotted name, e.g. `Str` or `time.Duration`
    Name(Ref),
    /// A constant, or `None` for an expression that isn't one a type can be
    Const {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        value: Option<ConstLit>,
    },
    /// Type arguments applied to a type, e.g. `Array[Int]`
    App {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        base: Box<TypeExpr>,
        args: Vec<TypeArg>,
        /// Its type, interned in the group it is written in
        #[serde(skip)]
        interned: Interned,
    },
    /// A schema, e.g. `{name: Str, ?port: Int}`. Where a type is required, it is
    /// `std.Dict` of itself, or `std.Tuple` of its items when it is `tuple`.
    Schema {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        params: Vec<TypeParam>,
        /// Whether it is written vertically without an explicitly keyed item, as an
        /// array's data is
        tuple: bool,
        /// Its collection, where kind checking found a type required
        #[serde(skip)]
        collection: Collection,
    },
    /// A parenthesized type
    Group {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        ty: Box<TypeExpr>,
    },
    /// A tuple type, e.g. `(Int, Str)`: `std.Tuple` of the schema of its items
    Tuple {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        params: Vec<TypeParam>,
    },
    /// A record type, e.g. `(name: Str, Int)`: `std.Record` of the schema of its
    /// items
    Record {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        params: Vec<TypeParam>,
    },
    /// An array type, e.g. `[Int]`: `std.Array` of its element type
    Array {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        elem: Box<TypeExpr>,
    },
    /// A union, e.g. `Str | Path`
    Union {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        members: Vec<TypeExpr>,
    },
    /// A function type, e.g. `(Int, ?Int) -> Int`
    Func {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
        params: Vec<TypeParam>,
        /// The type of the `<` implicit parameter, giving the ambient input
        input: Option<Box<TypeExpr>>,
        /// The type of the `>` implicit parameter, giving the ambient output
        output: Option<Box<TypeExpr>>,
        #[serde(skip, default = "wire::invalid")]
        arrow_span: Span,
        ret: Box<TypeExpr>,
        /// Its type, interned in the group it is written in
        #[serde(skip)]
        interned: Interned,
    },
    /// A type that could not be interpreted
    Error {
        #[serde(skip, default = "wire::invalid")]
        span: Span,
    },
}

/// A literal a constant type stands for
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ConstLit {
    Str(StrId),
    Int(i128),
    Bool(bool),
    Nil,
    Sym(Symbol),
}

/// A type argument in the `[]` of an application
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TypeArg {
    pub(crate) kind: TypeArgKind,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeArgKind {
    /// `T`
    Pos(TypeExpr),
    /// `name: T`
    Key { name: Symbol, ty: TypeExpr },
    /// `...T`, expanding a pack into further arguments
    Expand { ty: TypeExpr },
}

/// An item a schema or a function type's parameters declare
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TypeParam {
    /// How many of the element the item admits
    pub(crate) quant: Option<TypeQuant>,
    /// The element the quantifier applies to, absent only for a bare `*` or `**`
    pub(crate) kind: Option<TypeParamKind>,
}

/// How many of an element a schema item admits
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TypeQuant {
    /// `?`
    Opt,
    /// `*`
    Star,
    /// `**`
    StarStar,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeParamKind {
    /// `T`
    Pos(TypeExpr),
    /// `key: T`
    Key { key: TypeKey, ty: TypeExpr },
    /// `...S`
    Include { ty: TypeExpr },
    /// `...`, spanning it
    Open(#[serde(skip, default = "wire::invalid")] Span),
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeKey {
    /// A bareword key, which is a symbol
    Sym(Symbol),
    /// A key given by a type
    Type(Box<TypeExpr>),
}

impl TypeExpr {
    pub(crate) fn span(&self) -> Span {
        match self {
            TypeExpr::Name(name) => name.span,
            TypeExpr::Const { span, .. }
            | TypeExpr::App { span, .. }
            | TypeExpr::Schema { span, .. }
            | TypeExpr::Group { span, .. }
            | TypeExpr::Tuple { span, .. }
            | TypeExpr::Record { span, .. }
            | TypeExpr::Array { span, .. }
            | TypeExpr::Union { span, .. }
            | TypeExpr::Func { span, .. }
            | TypeExpr::Error { span } => *span,
        }
    }

    /// Visit each name within the type.
    pub(crate) fn names<'a>(&'a self, f: &mut impl FnMut(&'a Ref)) {
        self.each_node(&mut |ty| {
            if let TypeExpr::Name(name) = ty {
                f(name);
            }
        });
    }

    /// Visit the type and each type within it, outermost first.
    pub(crate) fn each_node<'a>(&'a self, f: &mut impl FnMut(&'a TypeExpr)) {
        f(self);
        match self {
            TypeExpr::Name(_) | TypeExpr::Const { .. } | TypeExpr::Error { .. } => {}
            TypeExpr::App { base, args, .. } => {
                base.each_node(f);
                for arg in args {
                    arg.ty().each_node(f);
                }
            }
            TypeExpr::Schema { params, .. }
            | TypeExpr::Tuple { params, .. }
            | TypeExpr::Record { params, .. } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    ty.each_node(f);
                }
            }
            TypeExpr::Group { ty, .. } | TypeExpr::Array { elem: ty, .. } => ty.each_node(f),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    member.each_node(f);
                }
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    ty.each_node(f);
                }
                for ty in [input, output].into_iter().flatten() {
                    ty.each_node(f);
                }
                ret.each_node(f);
            }
        }
    }

    /// The type with any parentheses around it removed
    pub(crate) fn ungrouped(&self) -> &TypeExpr {
        let mut ty = self;
        while let TypeExpr::Group { ty: inner, .. } = ty {
            ty = inner;
        }
        ty
    }
}

impl TypeArg {
    /// The argument's type.
    pub(crate) fn ty(&self) -> &TypeExpr {
        match &self.kind {
            TypeArgKind::Pos(ty) | TypeArgKind::Key { ty, .. } | TypeArgKind::Expand { ty } => ty,
        }
    }
}

impl TypeParam {
    /// The item's key type, if it has one, then its type.
    pub(crate) fn tys(&self) -> impl Iterator<Item = &TypeExpr> {
        let (key_ty, ty) = match &self.kind {
            Some(TypeParamKind::Pos(ty)) | Some(TypeParamKind::Include { ty }) => (None, Some(ty)),
            Some(TypeParamKind::Key { key, ty }) => (
                match key {
                    TypeKey::Sym(_) => None,
                    TypeKey::Type(key_ty) => Some(&**key_ty),
                },
                Some(ty),
            ),
            Some(TypeParamKind::Open(_)) | None => (None, None),
        };
        key_ty.into_iter().chain(ty)
    }
}

/// A name that stands for a type argument
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Binder {
    pub(crate) kind: BinderKind,
    pub(crate) name: Name,
    pub(crate) bound: Option<SiteId>,
    pub(crate) default: Option<SiteId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BinderKind {
    /// `T`
    Pos,
    /// `:K`
    Key,
    /// `...R`, `*R` or `**R`
    Rest(#[serde(with = "wire::rest_kind")] RestKind),
}

/// A parameter of a def, method or closure
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Param {
    pub(crate) kind: ParamKind,
    /// The bound name; absent for an anonymous rest or a sub-pattern
    pub(crate) name: Option<Name>,
    /// Whether it has a default, which makes it optional
    pub(crate) default: bool,
    pub(crate) annot: Option<SiteId>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum ParamKind {
    Pos,
    /// Passed by a symbol key
    Key {
        key: Name,
    },
    /// Passed by a constant key, or `None` for one no literal type can stand for
    ConstKey {
        key: Option<ConstLit>,
    },
    /// A rest, whose annotation is a type pattern when `pattern`
    Rest {
        #[serde(with = "wire::rest_kind")]
        kind: RestKind,
        pattern: bool,
    },
}

/// The signature of a def, method or closure
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Signature {
    pub(crate) params: Vec<Param>,
    /// The annotation of the `<` implicit parameter
    pub(crate) input: Option<SiteId>,
    /// The annotation of the `>` implicit parameter
    pub(crate) output: Option<SiteId>,
    pub(crate) ret: Option<SiteId>,
}

/// A def: a function's implementation or one of its `@def` overloads
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Def {
    pub(crate) name: Name,
    pub(crate) binders: Vec<Binder>,
    pub(crate) sig: Signature,
    /// Whether it is an overload signature, which has no body
    pub(crate) type_only: bool,
}

/// A method of a class
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Method {
    pub(crate) name: Name,
    pub(crate) binders: Vec<Binder>,
    pub(crate) sig: Signature,
    /// Whether it is an `@def` overload signature
    pub(crate) overload: bool,
    pub(crate) special: Option<SpecialMethod>,
    pub(crate) public: bool,
    /// Its namespace, from a `class` or `static` decorator
    pub(crate) scope: MemberScope,
    /// Its decorators, besides `class` and `static`
    pub(crate) decorators: Vec<Decorator>,
}

/// A decorator, as far as elaboration can tell what it is
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Decorator {
    /// A bare name
    Ident(Ref),
    /// Anything else
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MemberScope {
    Instance,
    Class,
    Static,
}

/// A field declaration, naming one or more fields that share an annotation
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Field {
    pub(crate) names: Vec<Name>,
    pub(crate) annot: Option<SiteId>,
    pub(crate) public: bool,
    pub(crate) scope: MemberScope,
}

/// A member of a class body, in source order
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Member {
    Field(Field),
    /// Signature `sig` of the method declaration `decl`
    Method {
        decl: DeclId,
        sig: usize,
    },
}

/// A supertype a class names
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Super {
    pub(crate) head: Ref,
    pub(crate) args: Vec<TypeArg>,
    /// The `[]` of the arguments, if written
    #[serde(skip)]
    pub(crate) bracket_span: Option<Span>,
    /// Whether it exists only in types
    pub(crate) type_only: bool,
    /// The supertype it names, interned in its class's group
    #[serde(skip)]
    pub(crate) interned: Interned,
}

/// A class or protocol
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Class {
    pub(crate) binders: Vec<Binder>,
    pub(crate) supers: Vec<Super>,
    pub(crate) members: Vec<Member>,
    pub(crate) decorators: Vec<Decorator>,
}

/// A transparent or opaque alias
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Alias {
    pub(crate) binders: Vec<Binder>,
    /// The aliased type, absent for an opaque alias
    pub(crate) body: Option<SiteId>,
}

/// A lambda or field initializer
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Closure {
    /// Where the closure is written
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
    pub(crate) sig: Signature,
}

impl<'u> Tables<'u> {
    /// A string of a unit's string table
    pub(crate) fn str(&self, unit: UnitId, id: StrId) -> &str {
        &self.strings[unit.index()][id.index()]
    }

    /// A name as written in `unit`
    pub(crate) fn name(&self, unit: UnitId, name: impl Into<Symbol>) -> &str {
        self.str(unit, name.into().text)
    }

    /// A name of `unit` as diagnostics quote it: as written in a unit with source,
    /// and otherwise by what it refers to
    pub(crate) fn quote(&self, unit: UnitId, name: Ref) -> String {
        if self.units[unit.index()].source.is_some() {
            return self.text(unit, name.span).to_owned();
        }
        match self.linked(name.link) {
            Some(&Referent::Decl(decl)) => {
                let decl_ref = &self.decls[decl.index()];
                match decl_ref.name {
                    Some(declared) => self.name(decl_ref.unit, declared).to_owned(),
                    None => "?".to_owned(),
                }
            }
            Some(&Referent::Binder(binder)) => {
                let written = &self.binders(binder.decl, binder.sig)[binder.slot];
                let unit = self.decls[binder.decl.index()].unit;
                self.name(unit, written.name).to_owned()
            }
            Some(&Referent::Module(module)) => self.units[module.index()].name(),
            Some(Referent::Value(_) | Referent::Error) | None => "?".to_owned(),
        }
    }

    /// The type a site interns
    pub(crate) fn site_ty(&self, site: SiteId) -> &TypeExpr {
        &self.sites[site.index()].ty
    }

    /// A type expression of `unit`, written in canonical form
    pub(crate) fn print(&self, unit: UnitId, ty: &TypeExpr) -> String {
        let mut out = String::new();
        let _ = Printer { tables: self, unit }.ty(ty, &mut out);
        out
    }
}

/// Writes type expressions in canonical form: one space after each `,` and `:` and
/// around each `|` and `->`, and none elsewhere
struct Printer<'t, 'u> {
    tables: &'t Tables<'u>,
    unit: UnitId,
}

impl Printer<'_, '_> {
    fn name(&self, text: StrId, out: &mut String) -> fmt::Result {
        out.write_str(self.tables.str(self.unit, text))
    }

    fn ty(&self, ty: &TypeExpr, out: &mut String) -> fmt::Result {
        match ty {
            TypeExpr::Name(name) => out.write_str(&self.tables.quote(self.unit, *name)),
            TypeExpr::Const { value, .. } => match value {
                Some(ConstLit::Str(value)) => {
                    write!(out, "{:?}", self.tables.str(self.unit, *value))
                }
                Some(ConstLit::Int(value)) => write!(out, "{value}"),
                Some(ConstLit::Bool(value)) => write!(out, "{value}"),
                Some(ConstLit::Nil) => out.write_str("nil"),
                Some(ConstLit::Sym(name)) => {
                    out.write_char(':')?;
                    self.name(name.text, out)?;
                    out.write_char(':')
                }
                None => out.write_char('?'),
            },
            TypeExpr::App { base, args, .. } => {
                self.ty(base, out)?;
                out.write_char('[')?;
                for (index, arg) in args.iter().enumerate() {
                    if index != 0 {
                        out.write_str(", ")?;
                    }
                    match &arg.kind {
                        TypeArgKind::Pos(ty) => self.ty(ty, out)?,
                        TypeArgKind::Key { name, ty } => {
                            self.name(name.text, out)?;
                            out.write_str(": ")?;
                            self.ty(ty, out)?;
                        }
                        TypeArgKind::Expand { ty } => {
                            out.write_str("...")?;
                            self.ty(ty, out)?;
                        }
                    }
                }
                out.write_char(']')
            }
            // A vertical schema that is a tuple prints as one
            TypeExpr::Schema {
                params, collection, ..
            } if collection.get() != Some(Designated::Intrinsic(Intrinsic::Tuple)) => {
                out.write_char('{')?;
                self.params(params, None, None, out)?;
                out.write_char('}')
            }
            TypeExpr::Group { ty, .. } => {
                out.write_char('(')?;
                self.ty(ty, out)?;
                out.write_char(')')
            }
            TypeExpr::Schema { params, .. }
            | TypeExpr::Tuple { params, .. }
            | TypeExpr::Record { params, .. } => {
                out.write_char('(')?;
                self.params(params, None, None, out)?;
                // A lone item without a quantifier would only group without its comma
                if let [
                    TypeParam {
                        quant: None,
                        kind: Some(TypeParamKind::Pos(_)),
                    },
                ] = params[..]
                {
                    out.write_char(',')?;
                }
                out.write_char(')')
            }
            TypeExpr::Array { elem, .. } => {
                out.write_char('[')?;
                self.ty(elem, out)?;
                out.write_char(']')
            }
            TypeExpr::Union { members, .. } => {
                for (index, member) in members.iter().enumerate() {
                    if index != 0 {
                        out.write_str(" | ")?;
                    }
                    self.ty(member, out)?;
                }
                Ok(())
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                out.write_char('(')?;
                self.params(params, input.as_deref(), output.as_deref(), out)?;
                out.write_str(") -> ")?;
                self.ty(ret, out)
            }
            TypeExpr::Error { .. } => out.write_char('?'),
        }
    }

    fn params(
        &self,
        params: &[TypeParam],
        input: Option<&TypeExpr>,
        output: Option<&TypeExpr>,
        out: &mut String,
    ) -> fmt::Result {
        let mut first = true;
        let mut sep = |out: &mut String| match std::mem::replace(&mut first, false) {
            true => Ok(()),
            false => out.write_str(", "),
        };
        for param in params {
            sep(out)?;
            match param.quant {
                Some(TypeQuant::Opt) => out.write_char('?')?,
                Some(TypeQuant::Star) => out.write_char('*')?,
                Some(TypeQuant::StarStar) => out.write_str("**")?,
                None => {}
            }
            match &param.kind {
                Some(TypeParamKind::Pos(ty)) => self.ty(ty, out)?,
                Some(TypeParamKind::Key { key, ty }) => {
                    match key {
                        TypeKey::Sym(name) => self.name(name.text, out)?,
                        TypeKey::Type(key) => self.ty(key, out)?,
                    }
                    out.write_str(": ")?;
                    self.ty(ty, out)?;
                }
                Some(TypeParamKind::Include { ty }) => {
                    out.write_str("...")?;
                    self.ty(ty, out)?;
                }
                Some(TypeParamKind::Open(_)) => out.write_str("...")?,
                None => {}
            }
        }
        for (sigil, ty) in [('<', input), ('>', output)] {
            if let Some(ty) = ty {
                sep(out)?;
                out.write_char(sigil)?;
                self.ty(ty, out)?;
            }
        }
        Ok(())
    }
}
