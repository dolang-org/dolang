//! Elaboration of the checked units' declarations into the type database.
//!
//! Passes run in order over common tables and the units' frozen syntax trees. The
//! tables refer to declaration nodes in place rather than copying source into an
//! intermediate representation.

mod collect;

use std::{
    collections::HashMap,
    fmt::{self, Write},
};

use super::r#type::{DeclId, DeclKind, UnitId, UnitSpan};
use crate::{
    Compiler, Unit,
    ast::{Class, Def, Function, Method, TypeAlias},
    diag::{NoteKind, Severity},
    source::{Diagnose, Note, Span},
};

pub(crate) use collect::collect;

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
}

/// A source declaration
pub(crate) struct Decl<'u> {
    pub(crate) unit: UnitId,
    pub(crate) kind: DeclKind,
    /// The declared name; absent for a closure
    pub(crate) name: Option<Span>,
    pub(crate) node: DeclNode<'u>,
    /// The declaration this one is nested in, whose binders it may capture
    pub(crate) outer: Option<DeclId>,
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
    /// Binder `slot` of signature `sig` of a declaration. `sig` indexes the
    /// declaration's defs or methods, and is 0 for any other declaration.
    Binder {
        decl: DeclId,
        sig: usize,
        slot: usize,
    },
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
    Binder {
        decl: DeclId,
        sig: usize,
        slot: usize,
    },
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
