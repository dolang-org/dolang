//! Document nodes recorded during elaboration.
//!
//! Elaboration builds a flat table of the declarations and constructs a document
//! extractor, language server, or highlighter would want to know about: what was
//! declared, where, and inside what.  Each node carries its parent, so structure
//! is available without walking the AST, and tokens carry a [`Id`] where they
//! would otherwise carry no structural information at all.
//!
//! The table is a plain arena.  Nodes are never looked up by content or by span:
//! a node is allocated once, at the point elaboration first learns of the
//! declaration, and every later reference reaches it through the resolution that
//! already exists ([`crate::ast::Var`], [`crate::ast::Res`],
//! [`crate::ast::Method`], [`crate::ast::FieldName`]).

use std::{
    num::NonZero,
    ops::{Index, IndexMut},
};

use dolang_util::{alias, arena::ArenaVec};

use crate::source::Span;

/// Identity of a document node: an index into a [`Table`].
///
/// Index 0 is reserved and never handed out, so an `Id` is never zero and
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
        self.0.get() as usize
    }

    pub(crate) fn from_index(index: usize) -> Self {
        Id(NonZero::new(index as u32).expect("index 0 is reserved"))
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
    },
    ImportItem {
        module: Span,
        item: Span,
        name: Span,
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

    /// A binding elaboration invented; never surfaced publicly
    Synthetic,
    /// The REPL's `_` binding; never surfaced publicly
    Repl,
}

impl Kind {
    /// Whether this kind is internal bookkeeping rather than something a
    /// consumer should see.
    pub(crate) fn is_internal(&self) -> bool {
        matches!(self, Kind::Synthetic | Kind::Repl)
    }

    /// The name the user wrote, for a kind declared by name in the source.
    ///
    /// A prelude import is named by configuration rather than by source text,
    /// so it has no span here.
    pub(crate) fn name(&self) -> Option<Span> {
        match self {
            Kind::Class { name, .. }
            | Kind::Function { name, .. }
            | Kind::Method { name, .. }
            | Kind::SpecialMethod { name }
            | Kind::Field { name, .. }
            | Kind::Bind { name, .. }
            | Kind::SelfParam { name }
            | Kind::ImportModule { name, .. }
            | Kind::ImportItem { name, .. } => Some(*name),
            Kind::PositionalParam { name, .. } | Kind::KeyParam { name, .. } => Some(*name),
            Kind::RestParam { name } => *name,
            _ => None,
        }
    }
}

/// A single document node.
#[derive(Debug)]
pub(crate) struct Node {
    /// The node this one is lexically inside, if any
    pub(crate) parent: Option<Id>,
    pub(crate) kind: Kind,
    /// The whole construct
    pub(crate) span: Span,
    /// Superseded, and not to be surfaced: an unused prelude import, or a
    /// binding whose origin was re-labelled by a later import of the same name
    pub(crate) dead: bool,
}

impl Node {
    pub(crate) fn new(parent: Option<Id>, kind: Kind, span: Span) -> Self {
        Self {
            parent,
            kind,
            span,
            dead: false,
        }
    }
}

/// The table of document nodes for a compilation unit.
pub(crate) struct Table {
    nodes: ArenaVec<Node>,
}

impl Table {
    /// Shared node for bindings elaboration invented
    pub(crate) const SYNTHETIC: Id = Id(NonZero::new(1).unwrap());
    /// Shared node for the REPL's `_` binding
    pub(crate) const REPL: Id = Id(NonZero::new(2).unwrap());

    pub(crate) fn new() -> Self {
        let table = Table {
            nodes: ArenaVec::new(),
        };
        // Slot 0 exists only to keep index 0 out of circulation, so that an
        // [`Id`] can be non-zero; nothing refers to it and `iter` skips it.
        table
            .nodes
            .push(Node::new(None, Kind::Synthetic, Span::INVALID));
        // Preallocate the two shared internal nodes so that classifying a
        // binding is an id comparison rather than a table lookup.
        table
            .nodes
            .push(Node::new(None, Kind::Synthetic, Span::INVALID));
        table.nodes.push(Node::new(None, Kind::Repl, Span::INVALID));
        table
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
