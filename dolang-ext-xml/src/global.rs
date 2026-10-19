use dolang::runtime::{
    Sym, Type,
    vm::{Register, StateTag},
};

use crate::{comment::Comment, document::Document};

use crate::element::{Element, TraverseIter};

pub(crate) struct Global<'v> {
    pub(crate) comment_type: Type<'v, Comment>,
    pub(crate) document_type: Type<'v, Document>,
    pub(crate) element_type: Type<'v, Element>,
    pub(crate) traverse_iter_type: Type<'v, TraverseIter>,
    pub(crate) syms: Syms<'v>,
}

pub(crate) struct Syms<'v> {
    pub(crate) attrs: Sym<'v, 'v>,
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) namespaces: Sym<'v, 'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            comment_type: builder.register_type::<Comment>(),
            document_type: builder.register_type::<Document>(),
            element_type: builder.register_type::<Element>(),
            traverse_iter_type: builder.register_type::<TraverseIter>(),
            syms: Syms {
                attrs: builder.sym("attrs"),
                namespace: builder.sym("namespace"),
                namespaces: builder.sym("namespaces"),
            },
        }
    }
}
