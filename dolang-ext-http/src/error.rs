//! `http.Error` and its subclasses, and the classification of a failed
//! request.

use std::{error::Error as StdError, io, iter, marker::PhantomData};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Strand, io as do_io,
    object::{TypeBuilder, fmt},
    value::fmt::Format,
};
use dolang_ext_url::create_url;

use crate::global::Tag;

/// The `kind` symbols of `TransportError`.
const TRANSPORT_KINDS: &[&str] = &["CONNECT", "TLS", "CLOSED", "IO"];
/// The `kind` symbols of `DataError`.
const DATA_KINDS: &[&str] = &["PARSE", "TOO_LARGE", "BODY"];
/// The `kind` symbols of `RequestError`.
const REQUEST_KINDS: &[&str] = &["REDIRECT", "OTHER"];

/// A class that can be raised, and the index of its `kind` symbol.
#[derive(Clone, Copy)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
enum Class {
    Transport(Transport),
    Data(Data),
    Request(Request),
}

#[derive(Clone, Copy)]
enum Transport {
    #[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
    Connect,
    #[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
    Tls,
    #[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
    Closed,
    Io,
}

#[derive(Clone, Copy)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
enum Data {
    Parse,
    TooLarge,
    Body,
}

#[derive(Clone, Copy)]
enum Request {
    Redirect,
    Other,
}

/// The errors in `err`'s source chain, starting with `err` itself.
fn chain(err: &reqwest::Error) -> impl Iterator<Item = &(dyn StdError + 'static)> {
    iter::successors(Some(err as &(dyn StdError + 'static)), |&err| err.source())
}

/// The innermost `io::Error` in `err`'s source chain.
pub(crate) fn bottom_io(err: &reqwest::Error) -> Option<&io::Error> {
    chain(err)
        .filter_map(|err| err.downcast_ref::<io::Error>())
        .last()
}

/// Converts a failure reading a body to the `io::Error` behind it, so that
/// the stream's error names the real cause.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) fn into_io(err: reqwest::Error) -> io::Error {
    match bottom_io(&err).map(|bottom| (bottom.kind(), bottom.raw_os_error())) {
        Some((_, Some(code))) => io::Error::from_raw_os_error(code),
        Some((kind, None)) => io::Error::new(kind, message(&err)),
        None => io::Error::other(message(&err)),
    }
}

/// The message for `err`: its source chain, leaving out a link whose text is
/// already part of the message, and replacing the message with a link that
/// repeats it.
pub(crate) fn message(err: &reqwest::Error) -> String {
    let mut message = String::new();
    for link in chain(err) {
        let text = link.to_string();
        if text.is_empty() || message.contains(&text) {
            continue;
        }
        if text.contains(&message) {
            message = text;
            continue;
        }
        message.push_str(": ");
        message.push_str(&text);
    }
    message
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn find<T: StdError + 'static>(err: &reqwest::Error) -> Option<&T> {
    chain(err).find_map(|err| err.downcast_ref::<T>())
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn classify(err: &reqwest::Error, bottom: Option<&io::Error>) -> Class {
    if err.is_redirect() {
        return Class::Request(Request::Redirect);
    }
    // TLS failures during connect also satisfy `is_connect`.
    if find::<native_tls::Error>(err).is_some() {
        return Class::Transport(Transport::Tls);
    }
    if let Some(hyper) = find::<hyper::Error>(err) {
        if hyper.is_parse_too_large() {
            return Class::Data(Data::TooLarge);
        }
        if hyper.is_parse() {
            return Class::Data(Data::Parse);
        }
        if hyper.is_incomplete_message() || hyper.is_closed() || hyper.is_canceled() {
            return Class::Transport(Transport::Closed);
        }
    }
    if err.is_connect() {
        return Class::Transport(Transport::Connect);
    }
    match bottom.map(io::Error::kind) {
        // hyper's body decoder reports bad framing with these.
        Some(io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput) => Class::Data(Data::Body),
        Some(io::ErrorKind::UnexpectedEof) => Class::Transport(Transport::Closed),
        Some(_) => Class::Transport(Transport::Io),
        None => Class::Request(Request::Other),
    }
}

/// The browser's fetch reports failures without detail.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn classify(err: &reqwest::Error, _bottom: Option<&io::Error>) -> Class {
    if err.is_redirect() {
        Class::Request(Request::Redirect)
    } else if err.is_request() || err.is_body() || err.is_decode() {
        Class::Transport(Transport::Io)
    } else {
        Class::Request(Request::Other)
    }
}

/// Raises `err` as the `http.Error` subclass it belongs to. A builder error
/// comes from the caller's own arguments, so it raises `ValueError`.
pub(crate) fn raise<'v, 's>(strand: &mut Strand<'v, 's>, err: reqwest::Error) -> Error<'v, 's> {
    let message = message(&err);
    if err.is_builder() {
        return Error::value(strand, message);
    }
    let bottom = bottom_io(&err);
    let class = classify(&err, bottom);
    let cause = bottom.map(|bottom| Cause {
        kind: bottom.kind(),
        os: bottom.raw_os_error(),
        message: bottom.to_string(),
    });
    let url = err.url().cloned();
    let types = &strand.state::<Tag>().types;
    let annex = |kind| ErrorAnnex {
        message,
        url,
        kind,
        cause,
    };
    match class {
        Class::Transport(kind) => Error::object_with_annex(
            strand,
            types.transport,
            HttpError::default(),
            annex(kind as usize),
        ),
        Class::Data(kind) => Error::object_with_annex(
            strand,
            types.data,
            HttpError::default(),
            annex(kind as usize),
        ),
        Class::Request(kind) => Error::object_with_annex(
            strand,
            types.request,
            HttpError::default(),
            annex(kind as usize),
        ),
    }
}

/// The `io::Error` behind a failure, kept so `cause` can raise it again.
pub(crate) struct Cause {
    kind: io::ErrorKind,
    os: Option<i32>,
    message: String,
}

impl Cause {
    fn to_io(&self) -> io::Error {
        match self.os {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(self.kind, self.message.clone()),
        }
    }
}

/// Names one of the `http` error classes, and the `kind` symbols it has.
pub(crate) trait ErrorClass<'v>: 'v {
    const NAME: &'v str;
    const KINDS: &'static [&'static str];
}

pub(crate) struct Base;
pub(crate) struct TransportClass;
pub(crate) struct DataClass;
pub(crate) struct RequestClass;

impl<'v> ErrorClass<'v> for Base {
    const NAME: &'v str = "Error";
    const KINDS: &'static [&'static str] = &[];
}

impl<'v> ErrorClass<'v> for TransportClass {
    const NAME: &'v str = "TransportError";
    const KINDS: &'static [&'static str] = TRANSPORT_KINDS;
}

impl<'v> ErrorClass<'v> for DataClass {
    const NAME: &'v str = "DataError";
    const KINDS: &'static [&'static str] = DATA_KINDS;
}

impl<'v> ErrorClass<'v> for RequestClass {
    const NAME: &'v str = "RequestError";
    const KINDS: &'static [&'static str] = REQUEST_KINDS;
}

/// One of the `http` error classes other than `StatusError`; they differ only
/// in name, place in the hierarchy, and `kind` symbols.
pub(crate) struct HttpError<C>(PhantomData<C>);

impl<C> Default for HttpError<C> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct ErrorAnnex {
    message: String,
    url: Option<url::Url>,
    /// The index of the `kind` symbol in the class's `KINDS`.
    kind: usize,
    cause: Option<Cause>,
}

impl<'v, C: ErrorClass<'v>> Object<'v> for HttpError<C> {
    const NAME: &'v str = C::NAME;
    const MODULE: &'v str = "http";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        if !C::KINDS.is_empty() {
            let kinds: Vec<_> = C::KINDS.iter().map(|name| builder.sym(name)).collect();
            builder = builder.get("kind", move |this, strand, out| {
                Output::set(strand, out, kinds[this.annex().kind]);
                Ok(())
            });
        }
        builder
            .get("url", |this, strand, out| {
                if let Some(url) = &this.annex().url {
                    create_url(strand, url.clone(), out);
                }
                Ok(())
            })
            .get("cause", |this, strand, out| {
                if let Some(cause) = &this.annex().cause {
                    let io = cause.to_io();
                    do_io::error(strand, io).get_value(strand, out);
                }
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<http.{} ", C::NAME)?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
