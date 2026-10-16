use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
};

use crate::url::Url;

pub(crate) struct Types<'v> {
    pub(crate) url: Type<'v, Url>,
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
        Self {
            types: Types {
                url: builder.register_type(),
            },
        }
    }
}
