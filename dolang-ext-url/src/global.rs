use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::url::{QueryIter, SegmentIter, Url};

pub(crate) struct Types<'v> {
    pub(crate) query_iter: Type<'v, QueryIter>,
    pub(crate) segment_iter: Type<'v, SegmentIter>,
    pub(crate) url: Type<'v, Url>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            types: Types {
                query_iter: builder.register_type(),
                segment_iter: builder.register_type(),
                url: builder.register_type(),
            },
        }
    }
}
