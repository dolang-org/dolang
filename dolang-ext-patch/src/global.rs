use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::patch::{ApplyError, ParseError, Patch, PatchIter};

pub(crate) struct Types<'v> {
    pub(crate) parse_error: Type<'v, ParseError>,
    pub(crate) apply_error: Type<'v, ApplyError>,
    pub(crate) patch: Type<'v, Patch<'v>>,
    pub(crate) patch_iter: Type<'v, PatchIter<'v>>,
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
                parse_error: builder.register_type(),
                apply_error: builder.register_type(),
                patch: builder.register_type(),
                patch_iter: builder.register_type(),
            },
        }
    }
}
