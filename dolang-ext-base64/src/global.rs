use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
};

use crate::base64::DecodeError;

pub(crate) struct Types<'v> {
    pub(crate) decode_error: Type<'v, DecodeError>,
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
                decode_error: builder.register_type(),
            },
        }
    }
}
