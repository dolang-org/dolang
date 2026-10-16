use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
};

use crate::glob::Glob;

pub(crate) struct Types<'v> {
    pub(crate) glob: Type<'v, Glob>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let types = Types {
            glob: builder.register_type(),
        };

        Self { types }
    }
}
