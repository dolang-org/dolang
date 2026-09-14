use dolang::runtime::{
    Sym, Type,
    vm::{Register, Stateful},
};

use crate::{
    attr::Attr,
    node::{Node, TraverseIter},
};

pub(crate) struct Global<'v> {
    pub(crate) attr_type: Type<'v, Attr>,
    pub(crate) node_type: Type<'v, Node>,
    pub(crate) traverse_iter_type: Type<'v, TraverseIter>,
    pub(crate) syms: Syms<'v>,
}

pub(crate) struct Syms<'v> {
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) prefix: Sym<'v, 'v>,
    pub(crate) attrs: Sym<'v, 'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            attr_type: builder.register_type::<Attr>(),
            node_type: builder.register_type::<Node>(),
            traverse_iter_type: builder.register_type::<TraverseIter>(),
            syms: Syms {
                namespace: builder.sym("namespace"),
                prefix: builder.sym("prefix"),
                attrs: builder.sym("attrs"),
            },
        }
    }
}
