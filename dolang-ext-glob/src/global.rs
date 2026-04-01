use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::glob::Glob;

pub(crate) struct Types<'v> {
    pub(crate) glob: Type<'v, Glob>,
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
        let types = Types {
            glob: builder.register_type(),
        };

        Self { types }
    }
}
