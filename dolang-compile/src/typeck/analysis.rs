//! What a check concluded about its units' spans, owned, with queries by position.
//!
//! Each unit's judgments are grouped by span into nodes. The spans nest like the
//! syntax tree they were copied from, so sorted by start ascending and end
//! descending, each node's parent is the innermost node before it containing it.

use std::{cmp::Reverse, iter, sync::Arc};

use super::{
    flow::Role,
    r#type::{Database, DeclId, Labels, Shared, Style, TypeId},
};
use crate::{UnitId, source::Span};

/// A check's own database, which its judgments' types are interned in, and the
/// names of its declarations
pub(crate) struct Types {
    pub(crate) db: Arc<Shared>,
    pub(crate) labels: Labels,
}

impl Types {
    pub(crate) fn render(&self, ty: TypeId, style: Style) -> String {
        Database::fork(&self.db).render(ty, &self.labels, style)
    }
}

/// What a check concluded about the spans of its units checked from source.
///
/// It holds no reference to the units, so it may outlive them. Spans are relative
/// to their unit's source; a query is answered without it.
pub struct Analysis {
    types: Arc<Types>,
    /// Each unit's tree, by [`UnitId`], for a unit checked from source
    units: Box<[Option<Tree>]>,
    /// Where each declaration of `types` is, by its ID there, unless it's a
    /// signature's
    places: Box<[Option<Place>]>,
}

/// Where a declaration is: its module's name and its name in the module
pub(crate) type Place = (Box<str>, Box<str>);

impl Analysis {
    pub(crate) fn new(
        types: Arc<Types>,
        units: Box<[Option<Tree>]>,
        places: Box<[Option<Place>]>,
    ) -> Self {
        Self {
            types,
            units,
            places,
        }
    }

    /// The innermost node of `unit` containing the byte `offset`, or `None` where
    /// there is none or `unit` wasn't checked from source.
    pub fn at(&self, unit: UnitId, offset: u32) -> Option<Node<'_>> {
        self.find(unit, offset, |span| {
            span.start <= offset && offset < span.end
        })
    }

    /// The innermost node of `unit` containing all of `span`, or `None` where
    /// there is none or `unit` wasn't checked from source.
    pub fn enclosing(&self, unit: UnitId, span: Span) -> Option<Node<'_>> {
        self.find(unit, span.start, |outer| {
            outer.start <= span.start && span.end <= outer.end
        })
    }

    /// The nodes of `unit` in source order, outer before inner. Empty where `unit`
    /// wasn't checked from source.
    pub fn nodes(&self, unit: UnitId) -> impl Iterator<Item = Node<'_>> {
        let tree = self.tree(unit);
        (0..tree.map_or(0, |tree| tree.nodes.len())).map(move |index| Node {
            analysis: self,
            tree: tree.expect("a unit with nodes has a tree"),
            index,
        })
    }

    fn tree(&self, unit: UnitId) -> Option<&Tree> {
        self.units.get(unit.index())?.as_ref()
    }

    /// The innermost node satisfying `contains` whose span starts at or before
    /// `start`
    fn find(&self, unit: UnitId, start: u32, contains: impl Fn(Span) -> bool) -> Option<Node<'_>> {
        let tree = self.tree(unit)?;
        let index = tree.find(start, contains)?;
        Some(Node {
            analysis: self,
            tree,
            index,
        })
    }
}

/// A unit's nodes, sorted by start ascending and end descending
pub(crate) struct Tree {
    nodes: Box<[NodeData]>,
}

struct NodeData {
    span: Span,
    /// The innermost other node containing it
    parent: Option<usize>,
    /// Sorted by role
    entries: Box<[Entry]>,
}

/// One judgment at a node's span
pub(crate) struct Entry {
    pub(crate) role: Role,
    /// The variable's name, or the member's
    pub(crate) name: Option<Box<str>>,
    pub(crate) ty: TypeId,
    pub(crate) unassigned: bool,
    /// The class declaring the member a member use reached
    pub(crate) decl: Option<DeclId>,
    /// The overload a call chose
    pub(crate) overload: Option<u32>,
}

impl Tree {
    /// The tree of judgments at spans, which nest without crossing
    pub(crate) fn new(mut judgments: Vec<(Span, Entry)>) -> Self {
        judgments.sort_by_key(|(span, entry)| (span.start, Reverse(span.end), entry.role));
        let mut nodes: Vec<NodeData> = Vec::new();
        // The nodes containing the last one, innermost last
        let mut open: Vec<usize> = Vec::new();
        let mut judgments = judgments.into_iter().peekable();
        while let Some((span, entry)) = judgments.next() {
            let entries: Vec<Entry> = iter::once(entry)
                .chain(iter::from_fn(|| {
                    judgments
                        .next_if(|&(next, _)| next == span)
                        .map(|(_, entry)| entry)
                }))
                .collect();
            while let Some(&top) = open.last()
                && !contains(nodes[top].span, span)
            {
                debug_assert!(
                    nodes[top].span.end <= span.start,
                    "spans {:?} and {span:?} cross",
                    nodes[top].span
                );
                open.pop();
            }
            open.push(nodes.len());
            nodes.push(NodeData {
                span,
                parent: open.iter().rev().nth(1).copied(),
                entries: entries.into(),
            });
        }
        Self {
            nodes: nodes.into(),
        }
    }

    /// The innermost node satisfying `contains` whose span starts at or before
    /// `start`. A node starting at or before it that doesn't is a finished
    /// sibling, so only the last one's ancestors are candidates.
    fn find(&self, start: u32, contains: impl Fn(Span) -> bool) -> Option<usize> {
        let last = self.nodes.partition_point(|node| node.span.start <= start);
        let mut index = last.checked_sub(1)?;
        loop {
            let node = &self.nodes[index];
            if contains(node.span) {
                return Some(index);
            }
            index = node.parent?;
        }
    }
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// A node of an [`Analysis`]: what the checker concluded about one span
#[derive(Clone, Copy)]
pub struct Node<'a> {
    analysis: &'a Analysis,
    tree: &'a Tree,
    index: usize,
}

impl<'a> Node<'a> {
    fn data(self) -> &'a NodeData {
        &self.tree.nodes[self.index]
    }

    /// Its span in its unit's source
    pub fn span(self) -> Span {
        self.data().span
    }

    /// The innermost other node containing it
    pub fn parent(self) -> Option<Self> {
        let index = self.data().parent?;
        Some(Self { index, ..self })
    }

    /// It and the nodes containing it, innermost first
    pub fn ancestors(self) -> impl Iterator<Item = Self> {
        iter::successors(Some(self), |node| node.parent())
    }

    /// Its judgments, by role
    pub fn judgments(self) -> impl Iterator<Item = Judged<'a>> {
        let analysis = self.analysis;
        self.data()
            .entries
            .iter()
            .map(move |entry| Judged { analysis, entry })
    }
}

/// One judgment of a [`Node`]
#[derive(Clone, Copy)]
pub struct Judged<'a> {
    analysis: &'a Analysis,
    entry: &'a Entry,
}

impl<'a> Judged<'a> {
    pub fn role(self) -> Role {
        self.entry.role
    }

    /// The name of the variable it reads or binds, if the variable has one, or
    /// of the member a member use reached
    pub fn name(self) -> Option<&'a str> {
        self.entry.name.as_deref()
    }

    /// The class declaring the member a member use reached
    pub fn declaration(self) -> Option<Declaration<'a>> {
        let decl = self.entry.decl?;
        let (module, name) = self.analysis.places.get(decl.index())?.as_ref()?;
        Some(Declaration { module, name })
    }

    /// The index of the overload a call chose, among its function's `@def`
    /// signatures in source order. `None` for a call of a function's
    /// implementation, or of a function with one signature.
    pub fn overload(self) -> Option<u32> {
        self.entry.overload
    }

    /// Whether some path reaches it without assigning the variable
    pub fn unassigned(self) -> bool {
        self.entry.unassigned
    }

    /// Its type, as `style` shows it: a member use's is the class declaring the
    /// member, and a call's the signature it reached. A read no path assigns
    /// sees no type.
    pub fn ty(self, style: Style) -> String {
        self.analysis.types.render(self.entry.ty, style)
    }

    /// Whether it saw no value at all
    pub(crate) fn none(self) -> bool {
        self.entry.ty == Database::fork(&self.analysis.types.db).bottom()
    }
}

/// A declaration, as a module's document index finds it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declaration<'a> {
    module: &'a str,
    name: &'a str,
}

impl<'a> Declaration<'a> {
    /// The name of the module that declares it
    pub fn module(self) -> &'a str {
        self.module
    }

    /// Its name in its module, qualified by the declarations it's nested in
    pub fn name(self) -> &'a str {
        self.name
    }
}

#[cfg(test)]
mod tests;
