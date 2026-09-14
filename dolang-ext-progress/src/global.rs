use std::cell::{Cell, RefCell};

use dolang::runtime::{
    Sym, Type,
    strand::LocalKey,
    value::Root,
    vm::{Register, Stateful},
};

use crate::progress::{Indicator, ProgressLocal};

pub(crate) struct UnitSymbols<'v> {
    pub(crate) count: Sym<'v, 'v>,
    pub(crate) bytes: Sym<'v, 'v>,
    pub(crate) percent: Sym<'v, 'v>,
}

pub(crate) struct Types<'v> {
    pub(crate) indicator: Type<'v, Indicator>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) units: UnitSymbols<'v>,
    pub(crate) local: LocalKey<'v, ProgressLocal>,
    /// The output captured when the sole active plain progress context begins.
    pub(crate) output: RefCell<Root<'v>>,
    pub(crate) plain_active: Cell<bool>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, ProgressLocal>) -> Self {
        Self {
            types: Types {
                indicator: builder.register_type(),
            },
            units: UnitSymbols {
                count: builder.sym("COUNT"),
                bytes: builder.sym("BYTES"),
                percent: builder.sym("PERCENT"),
            },
            local,
            output: RefCell::new(Root::new(builder)),
            plain_active: Cell::new(false),
        }
    }
}
