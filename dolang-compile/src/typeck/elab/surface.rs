//! The declaration surface: what elaboration reads of the checked units, copied out
//! of their syntax trees by collection.
//!
//! Nothing here refers to a syntax tree or to source text. Spans are kept, as the
//! keys the tables are indexed by and for diagnostics, and names are spelled through
//! each unit's string table, so a unit's surface means the same thing whether or not
//! its source is at hand.

use std::fmt::{self, Write};

use serde::{Deserialize, Serialize};

use super::Tables;
use crate::{
    RestKind,
    ast::SpecialMethod,
    source::Span,
    typeck::{
        r#type::{DeclId, UnitId},
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

/// A name as written, and where
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Name {
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
    pub(crate) text: StrId,
}

/// A type expression
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeExpr {
    /// A possibly dotted name, e.g. `Str` or `time.Duration`
    Name {
        #[serde(with = "wire::span")]
        span: Span,
        head: Name,
        fields: Vec<Name>,
    },
    /// A constant, or `None` for an expression that isn't one a type can be
    Const {
        #[serde(with = "wire::span")]
        span: Span,
        value: Option<ConstLit>,
    },
    /// Type arguments applied to a type, e.g. `Array[Int]`
    App {
        #[serde(with = "wire::span")]
        span: Span,
        base: Box<TypeExpr>,
        args: Vec<TypeArg>,
    },
    /// A schema, e.g. `{name: Str, ?port: Int}`
    Schema {
        #[serde(with = "wire::span")]
        span: Span,
        params: Vec<TypeParam>,
    },
    /// A parenthesized type
    Group {
        #[serde(with = "wire::span")]
        span: Span,
        ty: Box<TypeExpr>,
    },
    /// A union, e.g. `Str | Path`
    Union {
        #[serde(with = "wire::span")]
        span: Span,
        members: Vec<TypeExpr>,
    },
    /// A function type, e.g. `(Int, ?Int) -> Int`
    Func {
        #[serde(with = "wire::span")]
        span: Span,
        params: Vec<TypeParam>,
        /// The type of the `<` implicit parameter, giving the ambient input
        input: Option<Box<TypeExpr>>,
        /// The type of the `>` implicit parameter, giving the ambient output
        output: Option<Box<TypeExpr>>,
        #[serde(with = "wire::span")]
        arrow_span: Span,
        ret: Box<TypeExpr>,
    },
    /// A type that could not be interpreted
    Error {
        #[serde(with = "wire::span")]
        span: Span,
    },
}

/// A literal a constant type stands for
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ConstLit {
    Str(Box<str>),
    Int(i128),
    Bool(bool),
    Nil,
    Sym(Name),
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
    Key { name: Name, ty: TypeExpr },
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
    /// `...`
    Open,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TypeKey {
    /// A bareword key, which is a symbol
    Sym(Name),
    /// A key given by a type
    Type(Box<TypeExpr>),
}

impl TypeExpr {
    pub(crate) fn span(&self) -> Span {
        match self {
            TypeExpr::Name { span, .. }
            | TypeExpr::Const { span, .. }
            | TypeExpr::App { span, .. }
            | TypeExpr::Schema { span, .. }
            | TypeExpr::Group { span, .. }
            | TypeExpr::Union { span, .. }
            | TypeExpr::Func { span, .. }
            | TypeExpr::Error { span } => *span,
        }
    }

    /// Visit each name within the type: its head, then the fields dotted onto it.
    pub(crate) fn names<'a>(&'a self, f: &mut impl FnMut(&'a Name, &'a [Name])) {
        match self {
            TypeExpr::Name { head, fields, .. } => f(head, fields),
            TypeExpr::Const { .. } | TypeExpr::Error { .. } => {}
            TypeExpr::App { base, args, .. } => {
                base.names(f);
                for arg in args {
                    arg.ty().names(f);
                }
            }
            TypeExpr::Schema { params, .. } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    ty.names(f);
                }
            }
            TypeExpr::Group { ty, .. } => ty.names(f),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    member.names(f);
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
                    ty.names(f);
                }
                for ty in [input, output].into_iter().flatten() {
                    ty.names(f);
                }
                ret.names(f);
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
            Some(TypeParamKind::Open) | None => (None, None),
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
    pub(crate) decorators: Vec<Decorator>,
}

/// A decorator, as far as elaboration can tell what it is
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Decorator {
    /// A bare name
    Ident(Name),
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
    pub(crate) head: Name,
    pub(crate) fields: Vec<Name>,
    pub(crate) args: Vec<TypeArg>,
    /// The `[]` of the arguments, if written
    #[serde(with = "wire::opt_span")]
    pub(crate) bracket_span: Option<Span>,
    /// Whether it exists only in types
    pub(crate) type_only: bool,
}

impl Super {
    /// The span of the supertype's name, with its fields
    pub(crate) fn span(&self) -> Span {
        self.fields
            .last()
            .map_or(self.head.span, |field| self.head.span | field.span)
    }
}

/// A class or protocol
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Class {
    pub(crate) binders: Vec<Binder>,
    pub(crate) supers: Vec<Super>,
    pub(crate) members: Vec<Member>,
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
    pub(crate) fn str(&self, unit: UnitId, id: StrId) -> &'u str {
        self.strings[unit.index()][id.index()]
    }

    /// A name as written in `unit`
    pub(crate) fn name(&self, unit: UnitId, name: Name) -> &'u str {
        self.str(unit, name.text)
    }

    /// A possibly dotted name as written in `unit`
    pub(crate) fn dotted(&self, unit: UnitId, head: Name, fields: &[Name]) -> String {
        let mut out = self.name(unit, head).to_owned();
        for field in fields {
            out.push('.');
            out.push_str(self.name(unit, *field));
        }
        out
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
    fn name(&self, name: Name, out: &mut String) -> fmt::Result {
        out.write_str(self.tables.name(self.unit, name))
    }

    fn ty(&self, ty: &TypeExpr, out: &mut String) -> fmt::Result {
        match ty {
            TypeExpr::Name { head, fields, .. } => {
                out.write_str(&self.tables.dotted(self.unit, *head, fields))
            }
            TypeExpr::Const { value, .. } => match value {
                Some(ConstLit::Str(value)) => write!(out, "{value:?}"),
                Some(ConstLit::Int(value)) => write!(out, "{value}"),
                Some(ConstLit::Bool(value)) => write!(out, "{value}"),
                Some(ConstLit::Nil) => out.write_str("nil"),
                Some(ConstLit::Sym(name)) => {
                    out.write_char(':')?;
                    self.name(*name, out)?;
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
                            self.name(*name, out)?;
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
            TypeExpr::Schema { params, .. } => {
                out.write_char('{')?;
                self.params(params, None, None, out)?;
                out.write_char('}')
            }
            TypeExpr::Group { ty, .. } => {
                out.write_char('(')?;
                self.ty(ty, out)?;
                out.write_char(')')
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
                        TypeKey::Sym(name) => self.name(*name, out)?,
                        TypeKey::Type(key) => self.ty(key, out)?,
                    }
                    out.write_str(": ")?;
                    self.ty(ty, out)?;
                }
                Some(TypeParamKind::Include { ty }) => {
                    out.write_str("...")?;
                    self.ty(ty, out)?;
                }
                Some(TypeParamKind::Open) => out.write_str("...")?,
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
