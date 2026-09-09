//! Document structure built by the optional post-elaboration annotation pass.

use std::{
    num::NonZero,
    ops::{Index, IndexMut},
};

use dolang_util::alias;

use crate::source::Span;

mod comment;
mod index;
pub(crate) use index::index;

/// Identity of a document node: an index into a [`Table`].
///
/// An identity stores its zero-based table index plus one, so it is never zero and
/// `Option<Id>` is no wider than an `Id` — which parent links, jump targets and
/// decorator targets all are.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Id(NonZero<u32>);

impl Id {
    pub(crate) fn new(value: NonZero<u32>) -> Self {
        Id(value)
    }

    pub(crate) fn get(self) -> NonZero<u32> {
        self.0
    }

    pub(crate) fn index(self) -> usize {
        self.0.get() as usize - 1
    }

    pub(crate) fn from_index(index: usize) -> Self {
        Id(NonZero::new(u32::try_from(index + 1).expect("too many document nodes")).unwrap())
    }
}

/// A superclass reference.
///
/// A reference is a use site rather than a child, so it cannot be expressed by
/// parentage.  `target` is the node the reference resolves to when it is simply
/// an identifier, which is what gives a consumer the import provenance.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Super {
    pub(crate) span: Span,
    pub(crate) target: Option<Id>,
}

/// Everything about a node that varies by what kind of node it is.
///
/// A declared name and `pub` belong to the kinds that have them rather than to
/// every node, so a construct that declares nothing carries nothing.
#[derive(Debug)]
pub(crate) enum Kind {
    /// The complete source document.
    Root,

    // Declarations
    Class {
        name: Span,
        is_pub: bool,
        supers: alias::Box<[Super]>,
    },
    Function {
        name: Span,
        is_pub: bool,
    },
    Method {
        name: Span,
        is_pub: bool,
    },
    /// A method implementing a protocol, e.g. `(init)`
    ///
    /// It is part of the type's interface however it was declared, so it has no
    /// visibility to report.
    SpecialMethod {
        name: Span,
    },
    Field {
        name: Span,
        is_pub: bool,
    },
    Bind {
        name: Span,
        is_pub: bool,
    },
    PositionalParam {
        name: Span,
        default: Option<Span>,
    },
    KeyParam {
        key: Span,
        name: Span,
        default: Option<Span>,
    },
    RestParam {
        name: Option<Span>,
    },
    SelfParam {
        name: Span,
    },

    // Imports.  Prelude imports have no source text, so they carry their
    // resolved identity as strings rather than spans.
    ImportModule {
        module: Span,
        name: Span,
        is_pub: bool,
    },
    ImportItem {
        module: Span,
        item: Span,
        name: Span,
        is_pub: bool,
    },
    PreludeModule {
        module: alias::Box<str>,
        name: alias::Box<str>,
    },
    PreludeItem {
        module: alias::Box<str>,
        item: alias::Box<str>,
        name: alias::Box<str>,
    },

    // Structure
    Lambda,
    If,
    Else,
    While,
    For,
    Try,
    Catch,
    Finally,
    /// A comprehension `for`, as in vertical layout or an array/dict literal
    ForElem,
    /// A comprehension `if`
    IfElem,

    // References to a construct that is not an ancestor
    Decorator {
        target: Option<Id>,
    },
    Break {
        target: Option<Id>,
    },
    Continue {
        target: Option<Id>,
    },
    Return {
        target: Option<Id>,
    },
}

impl Kind {
    /// The name this node declares, where it declares one in source text.
    ///
    /// This is what go-to-definition jumps to and what an outline selects.  A
    /// node that declares nothing has none, and neither does a prelude binding,
    /// which is declared by configuration rather than by any text.  Whether a
    /// node has one also decides whether a declaration doc comment may attach
    /// to it. The root documentation block is assigned separately.
    pub(crate) fn definition(&self) -> Option<Span> {
        match self {
            Kind::Class { name, .. }
            | Kind::Function { name, .. }
            | Kind::Method { name, .. }
            | Kind::SpecialMethod { name }
            | Kind::Field { name, .. }
            | Kind::Bind { name, .. }
            | Kind::PositionalParam { name, .. }
            | Kind::KeyParam { name, .. }
            | Kind::SelfParam { name }
            | Kind::ImportModule { name, .. }
            | Kind::ImportItem { name, .. } => Some(*name),
            Kind::RestParam { name } => *name,
            Kind::Root
            | Kind::PreludeModule { .. }
            | Kind::PreludeItem { .. }
            | Kind::Lambda
            | Kind::If
            | Kind::Else
            | Kind::While
            | Kind::For
            | Kind::Try
            | Kind::Catch
            | Kind::Finally
            | Kind::ForElem
            | Kind::IfElem
            | Kind::Decorator { .. }
            | Kind::Break { .. }
            | Kind::Continue { .. }
            | Kind::Return { .. } => None,
        }
    }
}

/// A single document node.
#[derive(Debug)]
pub(crate) struct Node {
    /// The node this one is lexically inside, if any
    pub(crate) parent: Option<Id>,
    pub(crate) kind: Kind,
    /// The whole construct, from its first decorator to the end of its body
    pub(crate) span: Span,
    /// The doc comment block attached to this node, if any
    pub(crate) doc: Option<Span>,
}

impl Node {
    pub(crate) fn new(parent: Option<Id>, kind: Kind, span: Span, doc: Option<Span>) -> Self {
        Self {
            parent,
            kind,
            span,
            doc,
        }
    }
}

/// The table of document nodes for a compilation unit.
pub(crate) struct Table {
    nodes: Vec<Node>,
}

impl Table {
    pub(crate) fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Append a node, returning its identity.
    pub(crate) fn push(&mut self, node: Node) -> Id {
        let id = Id::from_index(self.nodes.len());
        self.nodes.push(node);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<Id> for Table {
    type Output = Node;

    fn index(&self, id: Id) -> &Node {
        &self.nodes[id.index()]
    }
}

impl IndexMut<Id> for Table {
    fn index_mut(&mut self, id: Id) -> &mut Node {
        &mut self.nodes[id.index()]
    }
}
