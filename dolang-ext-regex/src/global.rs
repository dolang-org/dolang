use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::regex::{Captures, Find, Match, Regex, RegexSplit};

pub(crate) struct Types<'v> {
    pub(crate) regex: Type<'v, Regex>,
    pub(crate) captures: Type<'v, Captures>,
    pub(crate) find: Type<'v, Find<'v>>,
    pub(crate) match_: Type<'v, Match>,
    pub(crate) split: Type<'v, RegexSplit<'v>>,
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
        let types = Types {
            regex: builder.register_type(),
            captures: builder.register_type(),
            find: builder.register_type(),
            match_: builder.register_type(),
            split: builder.register_type(),
        };

        Self { types }
    }
}
