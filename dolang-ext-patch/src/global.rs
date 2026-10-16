use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
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

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
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
