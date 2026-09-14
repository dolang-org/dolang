use dolang::runtime::{
    Type,
    vm::{Register, Stateful},
};

use crate::url::Url;

pub(crate) struct Types<'v> {
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
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            types: Types {
                url: builder.register_type(),
            },
        }
    }
}
