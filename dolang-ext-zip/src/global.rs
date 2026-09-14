use dolang::runtime::{
    Type,
    vm::{Register, Stateful},
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

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
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
