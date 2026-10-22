//! Document structure built by the optional post-elaboration annotation pass.

use std::{
    num::NonZero,
    ops::{Index, IndexMut},
};

use dolang_util::alias;

use crate::{BinderKind, ImplicitKind, RestKind, TypeQuant, source::Span};

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

/// Everything about a node that varies by what kind of node it is.
///
/// A declared name and `pub` belong to the kinds that have them rather than to
/// every node, so a construct that declares nothing carries nothing.
#[derive(Debug)]
pub(crate) enum Kind {
    /// The complete source document.
    Root,

    // Declarations
    /// A class, or with `type_only` a protocol
    Class {
        name: Span,
        is_pub: bool,
        type_only: bool,
    },
    /// A function, or with `type_only` an overload signature, which is public with its
    /// implementation
    Function {
        name: Span,
        is_pub: bool,
        type_only: bool,
    },
    Method {
        name: Span,
        is_pub: bool,
        type_only: bool,
    },
    /// A method implementing a protocol, e.g. `(init)`
    ///
    /// It is part of the type's interface however it was declared, so it has no
    /// visibility to report.
    SpecialMethod {
        name: Span,
        type_only: bool,
    },
    Field {
        name: Span,
        is_pub: bool,
    },
    Bind {
        name: Span,
        is_pub: bool,
    },
    Alias {
        name: Span,
        is_pub: bool,
        opaque: bool,
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
        kind: RestKind,
        name: Option<Span>,
        /// The explicit expansion marker in the annotation, if present.
        type_ellipsis: Option<Span>,
    },
    SelfParam {
        name: Span,
    },
    /// An implicit parameter naming an ambient channel, which binds no name
    ImplicitParam {
        kind: ImplicitKind,
    },

    // Imports.  Prelude imports have no source text, so they carry their
    // resolved identity as strings rather than spans, boxed so that they do not
    // set the size of every node.
    ImportModule {
        module: Span,
        name: Span,
        is_pub: bool,
        type_only: bool,
    },
    ImportItem {
        module: Span,
        item: Span,
        name: Span,
        is_pub: bool,
        type_only: bool,
    },
    PreludeModule(alias::Box<PreludeModule>),
    PreludeItem(alias::Box<PreludeItem>),

    // Structure
    Lambda,
    If,
    Else,
    /// A `match` arm
    Arm,
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

    // Types
    /// A type in an annotation, return type, alias body or superclass list, describing
    /// its parent, or with `type_only` a supertype the class does not inherit from at
    /// runtime
    Type {
        expr: Id,
        type_only: bool,
    },
    Binder {
        name: Span,
        kind: BinderKind,
        bound: Option<Id>,
        default: Option<Id>,
    },

    // Type forms. Each is a child of whatever refers to it, and its span includes any
    // parentheses that only group it.
    NameType {
        head: Span,
        target: Option<Id>,
    },
    ConstType(TypeConst),
    AppType {
        base: Id,
        /// [`Kind::TypeArg`] nodes, in written order
        args: alias::Box<[Id]>,
    },
    /// An argument in the `[]` of an application, without its trailing `,`
    TypeArg {
        kind: TypeArgKind,
        ty: Id,
    },
    SchemaType {
        /// [`Kind::TypeItem`] nodes, in written order
        items: alias::Box<[Id]>,
    },
    /// A tuple or record type, such as `(Int, Str)`
    ParensType {
        items: alias::Box<[Id]>,
    },
    /// An array type, such as `[Int]`
    ArrayType {
        elem: Id,
    },
    UnionType {
        members: alias::Box<[Id]>,
    },
    FuncType {
        items: alias::Box<[Id]>,
        /// The `<` implicit parameter, giving the ambient input
        input: Option<Id>,
        /// The `>` implicit parameter, giving the ambient output
        output: Option<Id>,
        ret: Id,
    },
    /// An item of a schema, tuple, record or function type's parameters, without its
    /// trailing `,`
    TypeItem {
        kind: TypeItemKind,
        /// Absent for `*`, `**` and `...` alone
        ty: Option<Id>,
    },
}

#[derive(Debug)]
pub(crate) struct PreludeModule {
    pub(crate) module: alias::Box<str>,
    pub(crate) name: alias::Box<str>,
}

#[derive(Debug)]
pub(crate) struct PreludeItem {
    pub(crate) type_only: bool,
    pub(crate) module: alias::Box<str>,
    pub(crate) item: alias::Box<str>,
    pub(crate) name: alias::Box<str>,
}

#[derive(Debug)]
pub(crate) enum TypeConst {
    Sym(alias::Box<str>),
    Str(alias::Box<str>),
    Int(i128),
    Bool(bool),
    Nil,
}

#[derive(Debug)]
pub(crate) enum TypeArgKind {
    Pos,
    Key { name: Span },
    Expand,
}

#[derive(Debug)]
pub(crate) enum TypeItemKind {
    Pos {
        quant: Option<TypeQuant>,
    },
    Key {
        quant: Option<TypeQuant>,
        /// The key as written, quotes and parentheses and all
        key: Span,
        /// The type giving the key, absent for a bareword symbol
        key_ty: Option<Id>,
    },
    AnyPos,
    AnyKey,
    Open,
}

impl Kind {
    /// The name this node declares, where it declares one in source text.
    ///
    /// This is what go-to-definition jumps to and what an outline selects.  A
    /// node that declares nothing has none, and neither does a prelude binding,
    /// which is declared by configuration rather than by any text.
    pub(crate) fn definition(&self) -> Option<Span> {
        match self {
            Kind::Class { name, .. }
            | Kind::Function { name, .. }
            | Kind::Method { name, .. }
            | Kind::SpecialMethod { name, .. }
            | Kind::Field { name, .. }
            | Kind::Bind { name, .. }
            | Kind::Alias { name, .. }
            | Kind::PositionalParam { name, .. }
            | Kind::KeyParam { name, .. }
            | Kind::SelfParam { name }
            | Kind::ImportModule { name, .. }
            | Kind::ImportItem { name, .. }
            | Kind::Binder { name, .. } => Some(*name),
            Kind::RestParam { name, .. } => *name,
            Kind::Root
            | Kind::PreludeModule(_)
            | Kind::PreludeItem(_)
            | Kind::Lambda
            | Kind::If
            | Kind::Else
            | Kind::Arm
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
            | Kind::Return { .. }
            | Kind::ImplicitParam { .. }
            | Kind::Type { .. }
            | Kind::NameType { .. }
            | Kind::ConstType(_)
            | Kind::AppType { .. }
            | Kind::TypeArg { .. }
            | Kind::SchemaType { .. }
            | Kind::ParensType { .. }
            | Kind::ArrayType { .. }
            | Kind::UnionType { .. }
            | Kind::FuncType { .. }
            | Kind::TypeItem { .. } => None,
        }
    }
}

/// A single document node.
#[derive(Debug)]
pub(crate) struct Node {
    /// The node this one is lexically inside, if any
    pub(crate) parent: Option<Id>,
    /// The first of this node's children, linked by [`Table::link`]
    pub(crate) first_child: Option<Id>,
    /// The next child of this node's parent, linked by [`Table::link`]
    pub(crate) next_sibling: Option<Id>,
    pub(crate) kind: Kind,
    /// The whole construct, from its first decorator to the end of its body. A
    /// prelude binding has no source text, and so has none.
    pub(crate) span: Option<Span>,
    /// The doc comment block attached to this node, if any
    pub(crate) doc: Option<Span>,
}

impl Node {
    pub(crate) fn new(
        parent: Option<Id>,
        kind: Kind,
        span: Option<Span>,
        doc: Option<Span>,
    ) -> Self {
        Self {
            parent,
            first_child: None,
            next_sibling: None,
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

    /// Drop the nodes from `len` on. Only valid before [`Table::link`], and only while
    /// nothing refers to the dropped nodes.
    pub(crate) fn truncate(&mut self, len: usize) {
        self.nodes.truncate(len);
    }

    /// Link every node into its parent's list of children.
    ///
    /// This runs once, after indexing, so nothing is linked while nodes may still be
    /// abandoned. Walking in reverse and prepending leaves each list in push order.
    pub(crate) fn link(&mut self) {
        for index in (0..self.nodes.len()).rev() {
            let id = Id::from_index(index);
            if let Some(parent) = self.nodes[index].parent {
                self.nodes[index].next_sibling = self[parent].first_child;
                self[parent].first_child = Some(id);
            }
        }
    }

    /// The children of `node`, as linked by [`Table::link`].
    pub(crate) fn children(&self, node: &Node) -> impl Iterator<Item = Id> + '_ {
        std::iter::successors(node.first_child, |child| self[*child].next_sibling)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_chains_children_in_push_order() {
        let mut table = Table::new();
        let root = table.push(Node::new(None, Kind::Root, None, None));
        let a = table.push(Node::new(Some(root), Kind::Root, None, None));
        let a1 = table.push(Node::new(Some(a), Kind::Root, None, None));
        let b = table.push(Node::new(Some(root), Kind::Root, None, None));
        let a2 = table.push(Node::new(Some(a), Kind::Root, None, None));
        table.link();
        assert_eq!(table.children(&table[root]).collect::<Vec<_>>(), [a, b]);
        assert_eq!(table.children(&table[a]).collect::<Vec<_>>(), [a1, a2]);
        assert_eq!(table.children(&table[b]).count(), 0);
        assert_eq!(table.children(&table[a1]).count(), 0);
    }
}
