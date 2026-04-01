use dolang::runtime::{
    Type,
    strand::LocalKey,
    vm::{Builder, Stateful},
};

use crate::progress::{Indicator, ProgressLocal};

pub(crate) struct Types<'v> {
    pub(crate) indicator: Type<'v, Indicator>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) local: LocalKey<'v, ProgressLocal>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            types: Types {
                indicator: builder.register_type(),
            },
            local: builder.local(),
        }
    }
}
