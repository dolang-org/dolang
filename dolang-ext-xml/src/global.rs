use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::node::{Node, TraverseIter};

pub(crate) struct Global<'v> {
    pub(crate) node_type: Type<'v, Node>,
    pub(crate) traverse_iter_type: Type<'v, TraverseIter>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            node_type: builder.register_type::<Node>(),
            traverse_iter_type: builder.register_type::<TraverseIter>(),
        }
    }
}
