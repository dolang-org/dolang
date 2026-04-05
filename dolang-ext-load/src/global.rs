use dolang::runtime::{
    Output,
    object::Type,
    value::{Empty, Root},
    vm::{Builder, Stateful},
};

use crate::load::ImportHandler;

pub(crate) struct Types<'v> {
    pub(crate) import_handler: Type<'v, ImportHandler>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) handlers: Root<'v>,
}

pub(crate) struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let mut root = Root::new(builder);
        Output::set(builder, &mut root, Empty::Dict);
        Self {
            types: Types {
                import_handler: builder.register_type(),
            },
            handlers: root,
        }
    }
}
