use dolang::runtime::{
    Sym, Type,
    vm::{Register, StateTag},
};

use crate::compress::{Compressor, Decompressor};

pub(crate) struct Types<'v> {
    pub(crate) compressor: Type<'v, Compressor>,
    pub(crate) decompressor: Type<'v, Decompressor>,
}

pub(crate) struct Syms<'v> {
    pub(crate) gzip: Sym<'v, 'v>,
    pub(crate) zstd: Sym<'v, 'v>,
    pub(crate) xz: Sym<'v, 'v>,
    pub(crate) auto: Sym<'v, 'v>,
    pub(crate) fastest: Sym<'v, 'v>,
    pub(crate) best: Sym<'v, 'v>,
    pub(crate) default: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            types: Types {
                compressor: builder.register_type(),
                decompressor: builder.register_type(),
            },
            syms: Syms {
                gzip: builder.sym("GZIP"),
                zstd: builder.sym("ZSTD"),
                xz: builder.sym("XZ"),
                auto: builder.sym("AUTO"),
                fastest: builder.sym("FASTEST"),
                best: builder.sym("BEST"),
                default: builder.sym("DEFAULT"),
            },
        }
    }
}
