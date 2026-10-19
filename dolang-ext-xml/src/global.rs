use dolang::runtime::{
    Sym, Type,
    vm::{Register, StateTag},
};

use crate::node::{Node, TraverseIter};

pub(crate) struct Global<'v> {
    pub(crate) node_type: Type<'v, Node>,
    pub(crate) traverse_iter_type: Type<'v, TraverseIter>,
    pub(crate) syms: Syms<'v>,
}

pub(crate) struct Syms<'v> {
    pub(crate) attrs: Sym<'v, 'v>,
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) namespaces: Sym<'v, 'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            node_type: builder.register_type::<Node>(),
            traverse_iter_type: builder.register_type::<TraverseIter>(),
            syms: Syms {
                attrs: builder.sym("attrs"),
                namespace: builder.sym("namespace"),
                namespaces: builder.sym("namespaces"),
            },
        }
    }
}
