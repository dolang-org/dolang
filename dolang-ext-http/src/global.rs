use std::collections::HashMap;

use dolang::runtime::{
    Sym, Type,
    vm::{Builder, Stateful},
};
use reqwest::Method;

use crate::{
    http::{ChunkIter, Client, ErrorObject, LineIter, Response, StatusObject},
    sse::{Event, EventIter},
};

pub(crate) struct Types<'v> {
    pub(crate) client: Type<'v, Client>,
    pub(crate) error: Type<'v, ErrorObject>,
    pub(crate) event: Type<'v, Event>,
    pub(crate) status: Type<'v, StatusObject>,
    pub(crate) response: Type<'v, Response>,
    pub(crate) chunk_iter: Type<'v, ChunkIter>,
    pub(crate) event_iter: Type<'v, EventIter>,
    pub(crate) line_iter: Type<'v, LineIter<'v>>,
}

pub(crate) struct Syms<'v> {
    pub(crate) body: Sym<'v, 'v>,
    pub(crate) content_type: Sym<'v, 'v>,
    pub(crate) cookies: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) filename: Sym<'v, 'v>,
    pub(crate) headers: Sym<'v, 'v>,
    #[cfg(feature = "json")]
    pub(crate) json: Sym<'v, 'v>,
    pub(crate) lines: Sym<'v, 'v>,
    pub(crate) multipart: Sym<'v, 'v>,
    pub(crate) name: Sym<'v, 'v>,
    pub(crate) query: Sym<'v, 'v>,
    pub(crate) status: Sym<'v, 'v>,
    pub(crate) ignore: Sym<'v, 'v>,
    #[cfg(feature = "json")]
    pub(crate) to_str: Sym<'v, 'v>,
    pub(crate) unix_socket: Sym<'v, 'v>,
    pub(crate) proxy: Sym<'v, 'v>,
    pub(crate) ca_cert: Sym<'v, 'v>,
    pub(crate) identity: Sym<'v, 'v>,
    pub(crate) password: Sym<'v, 'v>,
    pub(crate) invalid_certs: Sym<'v, 'v>,
    pub(crate) danger_accept: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
    pub(crate) http_methods: HashMap<Sym<'v, 'v>, Method>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let http_methods = HashMap::from_iter([
            (builder.sym("get"), Method::GET),
            (builder.sym("post"), Method::POST),
            (builder.sym("put"), Method::PUT),
            (builder.sym("delete"), Method::DELETE),
            (builder.sym("head"), Method::HEAD),
            (builder.sym("options"), Method::OPTIONS),
            (builder.sym("connect"), Method::CONNECT),
            (builder.sym("patch"), Method::PATCH),
            (builder.sym("trace"), Method::TRACE),
        ]);

        let error = builder.register_type::<ErrorObject>();

        Self {
            http_methods,
            types: Types {
                client: builder.register_type(),
                error,
                event: builder.register_type(),
                status: builder
                    .build_type::<StatusObject>((), ())
                    .nominal_supertype(error)
                    .build(),
                response: builder.register_type(),
                chunk_iter: builder.register_type(),
                event_iter: builder.register_type(),
                line_iter: builder.register_type(),
            },
            syms: Syms {
                body: builder.sym("body"),
                content_type: builder.sym("content_type"),
                cookies: builder.sym("cookies"),
                close: builder.sym("close"),
                filename: builder.sym("filename"),
                headers: builder.sym("headers"),
                #[cfg(feature = "json")]
                json: builder.sym("json"),
                lines: builder.sym("lines"),
                multipart: builder.sym("multipart"),
                name: builder.sym("name"),
                query: builder.sym("query"),
                status: builder.sym("status"),
                ignore: builder.sym("ignore"),
                #[cfg(feature = "json")]
                to_str: builder.sym("to_str"),
                unix_socket: builder.sym("unix_socket"),
                proxy: builder.sym("proxy"),
                ca_cert: builder.sym("ca_cert"),
                identity: builder.sym("identity"),
                password: builder.sym("password"),
                invalid_certs: builder.sym("invalid_certs"),
                danger_accept: builder.sym("DANGER_ACCEPT"),
            },
        }
    }
}
