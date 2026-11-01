use std::cell::Cell;

use dolang::runtime::{
    Output,
    object::Type,
    value::{Empty, Root},
    vm::{Register, StateTag},
};

use crate::load::{Bytecode, BytecodeError, ImportHandler};

pub(crate) struct Types<'v> {
    pub(crate) import_handler: Type<'v, ImportHandler>,
    pub(crate) bytecode: Type<'v, Bytecode>,
    pub(crate) bytecode_error: Type<'v, BytecodeError>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) handlers: Root<'v>,
    pub(crate) executors: Cell<usize>,
    pub(crate) gravestones: Cell<bool>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let mut root = Root::new(builder);
        Output::set(builder, &mut root, Empty::Array);
        Self {
            types: Types {
                import_handler: builder.register_type(),
                bytecode: builder.register_type(),
                bytecode_error: builder.register_type(),
            },
            handlers: root,
            executors: Cell::new(0),
            gravestones: Cell::new(false),
        }
    }
}
