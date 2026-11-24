use dolang::runtime::{
    Sym, Type,
    vm::{Register, StateTag},
};

use crate::rng::RngObj;

pub(crate) struct Types<'v> {
    pub(crate) rng: Type<'v, RngObj>,
}

pub(crate) struct Syms<'v> {
    pub(crate) alphabet: Sym<'v, 'v>,
    pub(crate) seed: Sym<'v, 'v>,
    pub(crate) algorithm: Sym<'v, 'v>,
    pub(crate) len: Sym<'v, 'v>,
    pub(crate) chacha8: Sym<'v, 'v>,
    pub(crate) chacha12: Sym<'v, 'v>,
    pub(crate) chacha20: Sym<'v, 'v>,
    pub(crate) xoshiro128pp: Sym<'v, 'v>,
    pub(crate) xoshiro256pp: Sym<'v, 'v>,
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
                rng: builder.register_type(),
            },
            syms: Syms {
                alphabet: builder.sym("alphabet"),
                seed: builder.sym("seed"),
                algorithm: builder.sym("algorithm"),
                len: builder.sym("len"),
                chacha8: builder.sym("CHACHA8"),
                chacha12: builder.sym("CHACHA12"),
                chacha20: builder.sym("CHACHA20"),
                xoshiro128pp: builder.sym("XOSHIRO128PP"),
                xoshiro256pp: builder.sym("XOSHIRO256PP"),
            },
        }
    }
}
