use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
};

use crate::zip::{Archive, Entry, File};

pub(crate) struct Types<'v> {
    pub(crate) archive: Type<'v, Archive>,
    pub(crate) entry: Type<'v, Entry>,
    pub(crate) file: Type<'v, File>,
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
                archive: builder.register_type(),
                entry: builder.register_type(),
                file: builder.register_type(),
            },
        }
    }
}
