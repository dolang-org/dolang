use dolang::runtime::{
    Sym, Type,
    vm::{Register, Stateful},
};

use crate::{guid::Guid, uuid::Uuid};

pub(crate) struct Types<'v> {
    pub(crate) uuid: Type<'v, Uuid>,
    pub(crate) guid: Type<'v, Guid>,
}

pub(crate) struct Syms<'v> {
    pub(crate) ncs: Sym<'v, 'v>,
    pub(crate) rfc4122: Sym<'v, 'v>,
    pub(crate) microsoft: Sym<'v, 'v>,
    pub(crate) future: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            types: Types {
                uuid: builder.register_type(),
                guid: builder.register_type(),
            },
            syms: Syms {
                ncs: builder.sym("NCS"),
                rfc4122: builder.sym("RFC4122"),
                microsoft: builder.sym("MICROSOFT"),
                future: builder.sym("FUTURE"),
            },
        }
    }
}
