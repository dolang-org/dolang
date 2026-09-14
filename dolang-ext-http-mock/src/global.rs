use dolang::runtime::{
    Sym, Type,
    vm::{Register, Stateful},
};

use crate::mock::{MockObject, RequestObject, Server};

pub(crate) struct Types<'v> {
    pub(crate) server: Type<'v, Server>,
    pub(crate) mock: Type<'v, MockObject>,
    pub(crate) request: Type<'v, RequestObject>,
}

pub(crate) struct Syms<'v> {
    pub(crate) method: Sym<'v, 'v>,
    pub(crate) path: Sym<'v, 'v>,
    pub(crate) path_regex: Sym<'v, 'v>,
    pub(crate) headers: Sym<'v, 'v>,
    pub(crate) query: Sym<'v, 'v>,
    pub(crate) body_json: Sym<'v, 'v>,
    pub(crate) match_kw: Sym<'v, 'v>,
    pub(crate) respond: Sym<'v, 'v>,
    pub(crate) status: Sym<'v, 'v>,
    pub(crate) body: Sym<'v, 'v>,
    #[cfg(feature = "json")]
    pub(crate) json: Sym<'v, 'v>,
    #[cfg(feature = "json")]
    pub(crate) encode: Sym<'v, 'v>,
    pub(crate) expect: Sym<'v, 'v>,
    pub(crate) name: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) cancel: Sym<'v, 'v>,
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
                server: builder.register_type(),
                mock: builder.register_type(),
                request: builder.register_type(),
            },
            syms: Syms {
                method: builder.sym("method"),
                path: builder.sym("path"),
                path_regex: builder.sym("path_regex"),
                headers: builder.sym("headers"),
                query: builder.sym("query"),
                body_json: builder.sym("body_json"),
                match_kw: builder.sym("match"),
                respond: builder.sym("respond"),
                status: builder.sym("status"),
                body: builder.sym("body"),
                #[cfg(feature = "json")]
                json: builder.sym("json"),
                #[cfg(feature = "json")]
                encode: builder.sym("encode"),
                expect: builder.sym("expect"),
                name: builder.sym("name"),
                close: builder.sym("close"),
                cancel: builder.sym("cancel"),
            },
        }
    }
}
