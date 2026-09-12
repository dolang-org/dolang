use std::{borrow::Cow, mem, pin::Pin, result, str};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::{object::fmt, strand::InterruptMask};

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use dolang::runtime::value::View;
use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value,
    call,
    error::{ErrorKind, ResultExt as _},
    method,
    object::{DictLike, DictView, DictViewSink, Mut, Ref, TypeBuilder},
    unpack,
    value::{BinEmbryo, TypeObject},
    vm::Builder,
};
use dolang_ext_time::{as_datetime, datetime};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use reqwest::tls::{Certificate, Identity};
use reqwest::{
    Method, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
    multipart,
};

use bstr::ByteSlice;
use bytes::Bytes;
use dolang_ext_url::{create_url, value_to_url};
use futures::stream::{Stream, StreamExt as _};

use crate::{
    body::IterBodies,
    global::Global,
    sse::{EventIter, SseParser},
};

pub(crate) struct Client {
    inner: Option<reqwest::Client>,
}

pub(crate) struct ErrorObject;
pub(crate) struct StatusObject;

pub(crate) struct ErrorAnnex {
    inner: reqwest::Error,
}

pub(crate) struct StatusAnnex {
    message: String,
    url: Option<url::Url>,
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
    truncated: bool,
}

pub(crate) struct ClientAnnex<'v> {
    global: State<'v, Global<'v>>,
}

const STATUS_BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusPolicy {
    Check,
    Ignore,
}

fn output_url<'v, 's>(strand: &mut Strand<'v, 's>, url: Option<&url::Url>, out: Slot<'v, '_>) {
    if let Some(url) = url {
        create_url(strand, url.clone(), out);
    }
}

fn header_value_to_str(value: &HeaderValue) -> Cow<'_, str> {
    match value.to_str() {
        Ok(value) => Cow::Borrowed(value),
        Err(_) => String::from_utf8_lossy(value.as_bytes()),
    }
}

fn header_value_from_slot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, HeaderValue> {
    if let Some(time) = as_datetime(strand, value) {
        return HeaderValue::from_str(&httpdate::fmt_http_date(time)).into_do(strand);
    }

    HeaderValue::from_bytes(value.to_string(strand)?.as_bytes()).into_do(strand)
}

fn header_value_or_datetime<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &HeaderValue,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let value_str = header_value_to_str(value);
    if let Ok(time) = httpdate::parse_http_date(value_str.as_ref()) {
        datetime(strand, time, &mut out).map_err(|err| Error::runtime(strand, err))
    } else {
        Output::set(strand, &mut out, value_str.as_ref());
        Ok(())
    }
}

fn header_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    headers: &HeaderMap,
    key: &Value<'v>,
    instance: i64,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, bool> {
    let Some(key) = key.as_str(strand) else {
        return Ok(false);
    };
    let Some(name) = strand.access(|x| HeaderName::from_bytes(key.as_str(x).as_bytes()).ok())
    else {
        return Ok(false);
    };
    let mut values = headers.get_all(&name).iter();
    let found = if instance >= 0 {
        values.nth(instance as usize)
    } else {
        let values: Vec<_> = values.collect();
        let index = values.len().checked_sub(instance.unsigned_abs() as usize);
        index.and_then(|index| values.get(index).copied())
    };
    match found {
        Some(value) => {
            header_value_or_datetime(strand, value, out)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn header_flatten<'v, 's>(
    strand: &mut Strand<'v, 's>,
    headers: &HeaderMap,
    sink: &mut DictViewSink<'v, '_>,
) -> Result<'v, 's, ()> {
    for (name, value) in headers {
        strand.with_slots_sync(|strand, [mut tmp]| {
            header_value_or_datetime(strand, value, Slot::reborrow(&mut tmp))?;
            sink.push(strand, name.as_str(), &tmp);
            Ok(())
        })?;
    }
    Ok(())
}

/// Lazy [`DictView`] projection over a [`StatusAnnex`]'s headers.
struct StatusHeaders;

impl<'v> DictLike<'v> for StatusHeaders {
    type Object = StatusObject;
    const MODULE: &'v str = "http";
    const NAME: &'v str = "Headers";

    fn len(&self, this: Instance<'v, '_, StatusObject>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().headers.iter().count()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, StatusObject>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        header_get(strand, &this.annex().headers, key, instance, out)
    }

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, StatusObject>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        header_flatten(strand, &this.annex().headers, sink)
    }
}

/// Lazy [`DictView`] projection over a [`Response`]'s headers.
struct ResponseHeaders;

impl<'v> DictLike<'v> for ResponseHeaders {
    type Object = Response;
    const MODULE: &'v str = "http";
    const NAME: &'v str = "Headers";

    fn len(&self, this: Instance<'v, '_, Response>, strand: &mut Strand<'v, '_>) -> usize {
        this.borrow(strand)
            .ok()
            .and_then(|borrow| {
                borrow
                    .inner
                    .as_ref()
                    .map(|inner| inner.headers.iter().count())
            })
            .unwrap_or(0)
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Response>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        let inner = borrow
            .inner
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "closed"))?;
        header_get(strand, &inner.headers, key, instance, out)
    }

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, Response>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner = borrow
            .inner
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "closed"))?;
        header_flatten(strand, &inner.headers, sink)
    }
}

fn status_message(status: reqwest::StatusCode, url: Option<&url::Url>) -> String {
    let prefix = if status.is_informational() {
        "HTTP status informational"
    } else if status.is_redirection() {
        "HTTP status redirection"
    } else if status.is_client_error() {
        "HTTP status client error"
    } else if status.is_server_error() {
        "HTTP status server error"
    } else {
        "HTTP status error"
    };

    let status_text = if let Some(reason) = status.canonical_reason() {
        format!("{} {}", status.as_u16(), reason)
    } else {
        status.as_u16().to_string()
    };

    let mut message = format!("{prefix} ({status_text})");
    if let Some(url) = url {
        message.push_str(" for url (");
        message.push_str(url.as_str());
        message.push(')');
    }
    message
}

fn invalid_status_policy<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::type_error(strand, r#"status: expected :IGNORE: or "IGNORE""#)
}

fn parse_status_policy<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, StatusPolicy> {
    if value.as_sym(strand) == Some(global.syms.ignore) {
        Ok(StatusPolicy::Ignore)
    } else if let Some(str) = value.as_str(strand)
        && strand.access(|x| str.as_str(x) == "IGNORE")
    {
        Ok(StatusPolicy::Ignore)
    } else {
        Err(invalid_status_policy(strand))
    }
}

async fn status_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mut response: ResponseParts,
) -> Error<'v, 's> {
    let status = response.status;
    let url = Some(response.url.clone());
    let headers = mem::take(&mut response.headers);
    let message = response
        .error_message
        .take()
        .unwrap_or_else(|| status_message(status, url.as_ref()));

    let mut body = Vec::new();
    let mut truncated = false;
    loop {
        if body.len() >= STATUS_BODY_LIMIT {
            match response.chunk().await {
                Ok(Some(_)) => truncated = true,
                Ok(None) => {}
                Err(_) => truncated = true,
            }
            break;
        }

        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = STATUS_BODY_LIMIT - body.len();
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    Error::object_with_annex(
        strand,
        global.types.status,
        StatusObject,
        StatusAnnex {
            message,
            url,
            status: status.as_u16(),
            headers,
            body,
            truncated,
        },
    )
}

fn status_text<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    annex: &'a StatusAnnex,
) -> Result<'v, 's, &'a str> {
    str::from_utf8(&annex.body).map_err(|_| Error::runtime(strand, "invalid UTF-8"))
}

impl<'v> Object<'v> for ErrorObject {
    const NAME: &'v str = "Error";
    const MODULE: &'v str = "http";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .nominal_supertype(TypeObject::RuntimeError)
            .get("url", |this, strand, out| {
                output_url(strand, this.annex().inner.url(), out);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().inner)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<http.Error ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for StatusObject {
    const NAME: &'v str = "Status";
    const MODULE: &'v str = "http";
    type Annex = StatusAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        #[cfg(feature = "json")]
        let mut builder = builder;

        #[cfg(feature = "json")]
        let decode = builder.sym("decode");

        #[cfg(feature = "json")]
        let builder =
            builder.method_with_slots("json", async move |this, strand, args, out, [mut json]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let text = status_text(strand, &annex)?;
                strand.import("json", &mut json).await?;
                method!(strand, json, decode, out, text).await
            });

        builder
            .get("url", |this, strand, out| {
                output_url(strand, this.annex().url.as_ref(), out);
                Ok(())
            })
            .get("status", |this, strand, out| {
                Output::set(strand, out, i64::from(this.annex().status));
                Ok(())
            })
            .get("headers", |this, strand, out| {
                Output::set(strand, out, DictView::new(this, StatusHeaders));
                Ok(())
            })
            .get("truncated", |this, strand, out| {
                Output::set(strand, out, this.annex().truncated);
                Ok(())
            })
            .method("body", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Output::set(strand, out, this.annex().body.as_slice());
                Ok(())
            })
            .method("text", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let input = status_text(strand, &annex)?;
                Output::set(strand, out, input);
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
        fmt!(strand, w, "<http.Status ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

fn reqwest_error<'v, 's>(strand: &mut Strand<'v, 's>, error: reqwest::Error) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    Error::object_with_annex(
        strand,
        global.types.error,
        ErrorObject,
        ErrorAnnex { inner: error },
    )
}

trait ErrorExt {
    fn into_http<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's>;
}

impl ErrorExt for reqwest::Error {
    fn into_http<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        reqwest_error(strand, self)
    }
}

pub(crate) trait ResultExt<T> {
    fn into_http<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T>;
}

impl<T> ResultExt<T> for result::Result<T, reqwest::Error> {
    fn into_http<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T> {
        self.map_err(|error| error.into_http(strand))
    }
}

fn multipart_text_field<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
    key: Sym<'v, '_>,
) -> Result<'v, 's, String> {
    let value = value.ok_or_else(|| Error::missing_key(strand, key))?;
    value
        .as_str(strand)
        .map(|value| value.to_string())
        .ok_or_else(|| {
            Error::type_error(
                strand,
                format!("multipart part {}: expected Str", key.as_str(strand.vm())),
            )
        })
}

async fn multipart_part<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    part_spec: &Value<'v>,
    bodies: &mut IterBodies<'v, '_, '_>,
) -> Result<'v, 's, (String, multipart::Part)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BodyKind {
        Body,
        #[cfg(feature = "json")]
        Json,
    }

    #[cfg(not(feature = "json"))]
    let _ = global;
    strand
        .with_slots(
            async move |strand,
                        [
                mut iter,
                mut item,
                mut key,
                mut value,
                mut body_iter,
                mut body,
            ]| {
                let mut name = None;
                let mut body_kind = None;
                let mut filename = None;
                let mut content_type = None;

                part_spec.iter(strand, &mut iter).await?;
                while iter.next(strand, &mut item).await? {
                    item.index(strand, 0, &mut key)?;
                    item.index(strand, 1, &mut value)?;
                    let key_sym = key.as_sym(strand.vm()).ok_or_else(|| {
                        Error::type_error(strand, "multipart part keys must be symbols")
                    })?;
                    if key_sym == global.syms.name {
                        name = Some(multipart_text_field(
                            strand,
                            Some(&value),
                            global.syms.name,
                        )?);
                    } else if key_sym == global.syms.body {
                        #[cfg(feature = "json")]
                        if body_kind == Some(BodyKind::Json) {
                            return Err(Error::runtime(
                                strand,
                                "multipart part may specify at most one of `body` or `json`",
                            ));
                        }
                        Output::set(strand, &mut body, &value);
                        body_kind = Some(BodyKind::Body);
                    } else if key_sym == global.syms.filename {
                        filename = Some(multipart_text_field(
                            strand,
                            Some(&value),
                            global.syms.filename,
                        )?);
                    } else if key_sym == global.syms.content_type {
                        content_type = Some(multipart_text_field(
                            strand,
                            Some(&value),
                            global.syms.content_type,
                        )?);
                    } else {
                        #[cfg(feature = "json")]
                        if key_sym == global.syms.json {
                            if body_kind == Some(BodyKind::Body) {
                                return Err(Error::runtime(
                                    strand,
                                    "multipart part may specify at most one of `body` or `json`",
                                ));
                            }
                            Output::set(strand, &mut body, &value);
                            body_kind = Some(BodyKind::Json);
                            continue;
                        }
                        return Err(Error::unexpected_key(strand, &key));
                    }
                }

                let name = name.ok_or_else(|| Error::missing_key(strand, global.syms.name))?;
                if body_kind.is_none() {
                    return Err(Error::missing_key(strand, global.syms.body));
                }

                #[cfg(feature = "json")]
                if body_kind == Some(BodyKind::Json) {
                    strand
                        .with_slots(async |strand, [mut json_mod, mut json_text]| {
                            strand.import("json", &mut json_mod).await?;
                            let encode = global.syms.encode;
                            method!(strand, json_mod, encode, &mut json_text, &mut body).await?;
                            Output::set(strand, &mut body, &json_text);
                            Ok(())
                        })
                        .await?;
                    if content_type.is_none() {
                        content_type = Some("application/json".to_owned());
                    }
                }

                let mut part = if let Some(slice) = body.as_bin(strand) {
                    multipart::Part::bytes(slice.to_vec())
                } else if let Some(text) = body.as_str(strand) {
                    multipart::Part::text(text.to_string())
                } else {
                    match body.iter(strand, &mut body_iter).await {
                        Ok(()) => {
                            multipart::Part::stream(bodies.add(strand, &body_iter, false).await?)
                        }
                        Err(e) if e.kind() == ErrorKind::Type => {
                            multipart::Part::text(body.to_string(strand)?)
                        }
                        Err(e) => return Err(e),
                    }
                };

                if let Some(filename) = filename {
                    part = part.file_name(filename);
                }
                if let Some(content_type) = content_type {
                    part = part.mime_str(&content_type).map_err(|err| {
                        Error::value(strand, format!("multipart part content_type: {err}"))
                    })?;
                }

                Ok((name, part))
            },
        )
        .await
}

async fn request<'v, 's>(
    client: &reqwest::Client,
    global: State<'v, Global<'v>>,
    st: &mut Strand<'v, 's>,
    method: Method,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    st.with_slots(
        async |st,
               [
            mut iter,
            mut item,
            mut key,
            mut value,
            mut body_iterator,
            mut body_roots,
            mut tmp,
        ]| {
            let mut url = None;
            let mut thunk = None;
            let mut body = None;
            let mut lines = None;
            let mut multipart = None;
            let mut status = StatusPolicy::Check;
            #[cfg(feature = "json")]
            let mut json = None;
            let mut headers = HeaderMap::new();
            let mut queries = Vec::new();
            for arg in args {
                match arg {
                    Arg::Pos(slot) => {
                        if url.is_none() {
                            url = Some(slot)
                        } else if thunk.is_none() {
                            thunk = Some(slot)
                        } else {
                            return Err(Error::unexpected_positional(st, 2));
                        }
                    }
                    Arg::Key(sym, slot) if sym == global.syms.body => body = Some(slot),
                    Arg::Key(sym, slot) if sym == global.syms.lines => lines = Some(slot),
                    Arg::Key(sym, slot) if sym == global.syms.multipart => multipart = Some(slot),
                    #[cfg(feature = "json")]
                    Arg::Key(sym, slot) if sym == global.syms.json => json = Some(slot),
                    Arg::Key(sym, slot) if sym == global.syms.headers => {
                        slot.iter(st, &mut iter).await?;
                        while iter.next(st, &mut item).await? {
                            item.index(st, 0, &mut key)?;
                            item.index(st, 1, &mut value)?;
                            let name = HeaderName::from_bytes(key.to_string(st)?.as_bytes())
                                .into_do(st)?;
                            let value = header_value_from_slot(st, &value)?;
                            headers.append(name, value);
                        }
                    }
                    Arg::Key(sym, slot) if sym == global.syms.query => {
                        slot.iter(st, &mut iter).await?;
                        while iter.next(st, &mut item).await? {
                            item.index(st, 0, &mut key)?;
                            item.index(st, 1, &mut value)?;
                            queries.push((key.to_string(st)?, value.to_string(st)?));
                        }
                    }
                    Arg::Key(sym, slot) if sym == global.syms.status => {
                        status = parse_status_policy(st, global, &slot)?
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(st, sym)),
                }
            }
            #[cfg(feature = "json")]
            if body.is_some() as usize
                + lines.is_some() as usize
                + json.is_some() as usize
                + multipart.is_some() as usize
                > 1
            {
                return Err(Error::runtime(
                    st,
                    "at most one of `body`, `lines`, `json`, or `multipart` arguments may be specified",
                ));
            }
            #[cfg(not(feature = "json"))]
            if body.is_some() as usize + lines.is_some() as usize + multipart.is_some() as usize > 1
            {
                return Err(Error::runtime(
                    st,
                    "at most one of `body`, `lines`, or `multipart` arguments may be specified",
                ));
            }
            let url = url.ok_or_else(|| Error::missing_positional(st, 0))?;
            let url = value_to_url(st, &url)?;
            let mut builder = client.request(method, url);

            let mut bodies = IterBodies::new(&mut body_roots);

            if let Some(body) = body {
                // Try direct conversions first (backward compatibility)
                if let Some(slice) = body.as_bin(st) {
                    // Direct binary - no streaming needed
                    builder = builder.body(slice.to_vec());
                } else if let Some(str) = body.as_str(st) {
                    // Direct string - no streaming needed
                    builder = builder.body(str.to_string());
                } else {
                    // Try to get an iterator for streaming
                    match body.iter(st, &mut body_iterator).await {
                        Ok(()) => {
                            // We have an iterator - stream it where the target can
                            builder = builder.body(bodies.add(st, &body_iterator, false).await?);
                        }
                        Err(e) if e.kind() == ErrorKind::Type => {
                            // Not iterable - convert to string
                            builder = builder.body(body.to_string(st)?);
                        }
                        Err(e) => return Err(e),
                    }
                }
            }

            #[cfg(feature = "json")]
            if let Some(json) = json {
                st.import("json", &mut key).await?;
                let encode = global.syms.encode;
                method!(st, key, encode, &mut value, json).await?;
                builder = builder
                    .body(value.to_string(st)?)
                    .header("content-type", "application/json");
            }

            if let Some(lines) = lines {
                lines.iter(st, &mut body_iterator).await?;
                builder = builder.body(bodies.add(st, &body_iterator, true).await?);
            }

            if let Some(multipart) = multipart {
                if headers.contains_key("content-type") {
                    return Err(Error::runtime(
                        st,
                        "multipart requests must not set content-type manually",
                    ));
                }

                let mut form = multipart::Form::new();
                multipart.iter(st, &mut iter).await?;
                while iter.next(st, &mut item).await? {
                    let (name, part) = multipart_part(st, global, &item, &mut bodies).await?;
                    form = form.part(name, part);
                }
                builder = builder.multipart(form);
            }

            if !queries.is_empty() {
                builder = builder.query(&queries);
            }
            builder = builder.headers(headers);

            // Execute request, pumping any streamed bodies concurrently
            let response = ResponseParts::new(bodies.send(st, builder).await?);
            if status == StatusPolicy::Check && !response.status.is_success() {
                return Err(status_error(st, global, response).await);
            }
            let response = Response {
                inner: Some(response),
            };
            global
                .types
                .response
                .create_with_annex(st, response, global, &mut value);
            if let Some(thunk) = thunk {
                let res = call!(st, thunk, out, &value).await;
                let _ = st
                    .with_interrupt_mask(InterruptMask::all(), async move |st| {
                        method!(st, value, global.syms.close, &mut tmp).await
                    })
                    .await;
                res
            } else {
                Output::set(st, out, value);
                Ok(())
            }
        },
    )
    .await
}

impl<'v> Object<'v> for Client {
    const NAME: &'v str = "Client";
    const MODULE: &'v str = "http";
    type Annex = ClientAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let unix_socket = global.syms.unix_socket;
        let proxy = global.syms.proxy;
        let cookies = global.syms.cookies;
        let ca_cert = global.syms.ca_cert;
        let identity = global.syms.identity;
        let password = global.syms.password;
        let invalid_certs = global.syms.invalid_certs;
        let (
            [],
            [
                func,
                unix_socket,
                proxy,
                cookies,
                ca_cert,
                identity,
                password,
                invalid_certs,
            ],
        ) = unpack!(
            strand,
            args,
            0,
            1,
            unix_socket = None,
            proxy = None,
            cookies = None,
            ca_cert = None,
            identity = None,
            password = None,
            invalid_certs = None
        )?;
        // Fetch has no equivalent for these options.
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        let builder = {
            let options = [
                ("unix_socket", unix_socket),
                ("proxy", proxy),
                ("cookies", cookies),
                ("ca_cert", ca_cert),
                ("identity", identity),
                ("password", password),
                ("invalid_certs", invalid_certs),
            ];
            if let Some((name, _)) = options.into_iter().find(|(_, value)| value.is_some()) {
                return Err(Error::runtime(
                    strand,
                    format!("{name} is not supported in the browser"),
                ));
            }
            reqwest::ClientBuilder::new()
        };

        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        let builder = {
            let mut builder = reqwest::ClientBuilder::new();

            builder = if let Some(unix_socket) = unix_socket {
                #[cfg(unix)]
                {
                    builder.unix_socket(
                        unix_socket
                            .as_str(strand)
                            .ok_or_else(|| Error::type_error(strand, "unix_socket: expected Str"))?
                            .to_string(),
                    )
                }
                #[cfg(not(unix))]
                {
                    let _ = unix_socket;
                    return Err(Error::runtime(strand, "unix_socket only available on Unix"));
                }
            } else {
                builder
            };

            if let Some(proxy) = proxy {
                if proxy.is_nil() {
                    builder = builder.no_proxy();
                } else {
                    let url = value_to_url(strand, &proxy)?;
                    builder = builder.proxy(reqwest::Proxy::all(url.as_str()).into_http(strand)?);
                }
            }

            if let Some(cookies) = cookies {
                builder = builder.cookie_store(
                    cookies
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "cookies: expected Bool"))?,
                );
            }

            if let Some(ca_cert) = ca_cert {
                let cert = match ca_cert.view(strand) {
                    View::Str(s) => {
                        strand.access(|x| Certificate::from_pem(s.as_str(x).as_bytes()))
                    }
                    View::Bin(b) => strand.access(|x| Certificate::from_pem(b.as_slice(x))),
                    _ => return Err(Error::type_error(strand, "ca_cert: expected Str or Bin")),
                }
                .into_http(strand)?;
                builder = builder.add_root_certificate(cert);
            }

            if let Some(identity) = identity {
                let id_bytes = identity
                    .as_bin(strand)
                    .ok_or_else(|| Error::type_error(strand, "identity: expected Str or Bin"))?;
                let pass = match password {
                    Some(p) => p
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "password: expected Str"))?
                        .to_string(),
                    None => String::new(),
                };
                let id = strand
                    .access(|x| Identity::from_pkcs12_der(id_bytes.as_slice(x), &pass))
                    .into_http(strand)?;
                builder = builder.identity(id);
            } else if password.is_some() {
                return Err(Error::value(strand, "password requires identity"));
            }

            if let Some(invalid_certs) = invalid_certs {
                let sym = invalid_certs
                    .as_sym(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "invalid_certs: expected symbol"))?;
                if sym != global.syms.danger_accept {
                    return Err(Error::value(
                        strand,
                        "invalid_certs: expected :DANGER_ACCEPT:",
                    ));
                }
                builder = builder.danger_accept_invalid_certs(true);
            }

            builder
        };

        if let Some(func) = func {
            strand
                .with_slots(async move |strand, [mut client, tmp]| {
                    let value = Client {
                        inner: Some(builder.build().into_http(strand)?),
                    };
                    this.create_with_annex(strand, value, ClientAnnex { global }, &mut client);
                    let res = call!(strand, func, out, &client).await;
                    let _ = strand
                        .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                            method!(strand, client, global.syms.close, tmp).await
                        })
                        .await;
                    res
                })
                .await
        } else {
            let value = Client {
                inner: Some(builder.build().into_http(strand)?),
            };
            this.create_with_annex(strand, value, ClientAnnex { global }, out);
            Ok(())
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("close", async move |this, strand, args, _out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            drop(this.borrow_mut(strand)?.inner.take());
            Ok(())
        })
    }

    async fn method<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let client = this
            .borrow(strand)?
            .inner
            .clone()
            .ok_or_else(|| Error::state_error(strand, "client closed"))?;
        if let Some(method) = annex.global.http_methods.get(&method) {
            request(&client, annex.global, strand, method.clone(), args, out).await
        } else {
            Err(Error::field(strand, method))
        }
    }
}

pub(crate) struct Response {
    pub(crate) inner: Option<ResponseParts>,
}

/// Body of a response as a stream of chunks.
type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>>>>;

/// An open response: its metadata and unread body.
///
/// The body is read through `bytes_stream`, the one body API that reqwest's
/// native and browser backends share.
pub(crate) struct ResponseParts {
    status: StatusCode,
    url: url::Url,
    headers: HeaderMap,
    /// reqwest's message for a failure status, captured before the body is taken.
    error_message: Option<String>,
    body: ByteStream,
}

impl ResponseParts {
    fn new(mut response: reqwest::Response) -> Self {
        let error_message = response
            .error_for_status_ref()
            .err()
            .map(|err| err.to_string());
        Self {
            status: response.status(),
            url: response.url().clone(),
            headers: mem::take(response.headers_mut()),
            error_message,
            body: Box::pin(response.bytes_stream()),
        }
    }

    /// Reads the next chunk of the body, or `None` at its end.
    pub(crate) async fn chunk(&mut self) -> reqwest::Result<Option<Bytes>> {
        self.body.next().await.transpose()
    }

    /// Reads the rest of the body.
    async fn bytes(mut self) -> reqwest::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Reads the rest of the body as text, replacing invalid UTF-8.
    async fn text(self) -> reqwest::Result<String> {
        let bytes = self.bytes().await?;
        Ok(String::from_utf8(bytes)
            .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned()))
    }
}

impl<'v> Object<'v> for Response {
    const NAME: &'v str = "Response";
    const MODULE: &'v str = "http";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let thunk = match args.next() {
            Some(Arg::Pos(slot)) => slot,
            Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            None => return Err(Error::missing_positional(strand, 0)),
        };
        let res = thunk.call(strand, args, out).await;
        drop(this.borrow_mut(strand)?.inner.take());
        res
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        #[cfg(feature = "json")]
        let mut builder = builder;

        #[cfg(feature = "json")]
        let decode = builder.sym("decode");

        #[cfg(feature = "json")]
        let builder =
            builder.method_with_slots("json", async move |this, strand, args, out, [mut json]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let inner = if let Some(inner) = borrow.inner.take() {
                    inner
                } else {
                    return Err(Error::state_error(strand, "closed"));
                };
                let text = inner.text().await.into_http(strand)?;
                strand.import("json", &mut json).await?;
                method!(strand, json, decode, out, text.as_str()).await
            });

        builder
            .get("url", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let inner = borrow
                    .inner
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "closed"))?;
                create_url(strand, inner.url.clone(), out);
                Ok(())
            })
            .get("status", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                if borrow.inner.is_none() {
                    return Err(Error::state_error(strand, "closed"));
                }
                Output::set(strand, out, borrow.inner.as_ref().unwrap().status.as_u16());
                Ok(())
            })
            .get("headers", |this, strand, out| {
                Output::set(strand, out, DictView::new(this, ResponseHeaders));
                Ok(())
            })
            .method("body", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let inner = if let Some(inner) = borrow.inner.take() {
                    inner
                } else {
                    return Err(Error::state_error(strand, "closed"));
                };
                let res = inner.bytes().await.into_http(strand)?;
                Output::set(strand, out, res.as_slice());
                Ok(())
            })
            .method("text", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let inner = if let Some(inner) = borrow.inner.take() {
                    inner
                } else {
                    return Err(Error::state_error(strand, "closed"));
                };
                let res = inner.text().await.into_http(strand)?;
                Output::set(strand, out, res.as_str());
                Ok(())
            })
            .method("throw_for_status", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = *this.annex();
                let mut borrow = this.borrow_mut(strand)?;
                let Some(inner) = borrow.inner.take() else {
                    return Err(Error::state_error(strand, "closed"));
                };
                if inner.status.is_success() {
                    borrow.inner = Some(inner);
                    drop(borrow);
                    Ok(())
                } else {
                    drop(borrow);
                    Err(status_error(strand, global, inner).await)
                }
            })
            .method_with_slots(
                "chunks",
                async move |this, strand, args, out, [mut iter]| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    let global = *this.annex();
                    let borrow = this.borrow(strand)?;
                    if borrow.inner.is_none() {
                        return Err(Error::state_error(strand, "closed"));
                    }
                    drop(borrow);
                    global
                        .types
                        .chunk_iter
                        .create_with_annex(strand, ChunkIter, global, &mut iter);
                    // Store the response object in slot 0 of the iterator
                    global.types.chunk_iter.cast(&iter).unwrap().enter_sync(
                        strand,
                        |strand, iterator| {
                            let mut iter_borrow = iterator.borrow_mut(strand)?;
                            Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                            Ok(())
                        },
                    )?;
                    Output::set(strand, out, iter);
                    Ok(())
                },
            )
            .method_with_slots(
                "events",
                async move |this, strand, args, out, [mut iter]| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    let global = *this.annex();
                    let borrow = this.borrow(strand)?;
                    if borrow.inner.is_none() {
                        return Err(Error::state_error(strand, "closed"));
                    }
                    drop(borrow);
                    global.types.event_iter.create_with_annex(
                        strand,
                        EventIter {
                            parser: SseParser::default(),
                        },
                        global,
                        &mut iter,
                    );
                    global.types.event_iter.cast(&iter).unwrap().enter_sync(
                        strand,
                        |strand, iterator| {
                            let mut iter_borrow = iterator.borrow_mut(strand)?;
                            Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                            Ok(())
                        },
                    )?;
                    Output::set(strand, out, iter);
                    Ok(())
                },
            )
            .method_with_slots("lines", async move |this, strand, args, out, [mut iter]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = *this.annex();
                let borrow = this.borrow(strand)?;
                if borrow.inner.is_none() {
                    return Err(Error::state_error(strand, "closed"));
                }
                drop(borrow);
                global.types.line_iter.create_with_annex(
                    strand,
                    LineIter::new(),
                    global,
                    &mut iter,
                );
                // Store the response object in slot 0 of the iterator
                global.types.line_iter.cast(&iter).unwrap().enter_sync(
                    strand,
                    |strand, iterator| {
                        let mut iter_borrow = iterator.borrow_mut(strand)?;
                        Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                        Ok(())
                    },
                )?;
                Output::set(strand, out, iter);
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                drop(this.borrow_mut(strand)?.inner.take());
                Ok(())
            })
    }
}

pub(crate) struct ChunkIter;

impl<'v> Object<'v> for ChunkIter {
    const NAME: &'v str = "ResponseChunkIter";
    const MODULE: &'v str = "http";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = *this.annex();

        strand
            .with_slots(async move |strand, [mut response]| {
                let borrow = this.borrow(strand)?;

                // Get the response object from slot 0
                Output::set(strand, &mut response, Ref::slot::<0>(&borrow));
                drop(borrow);
                global
                    .types
                    .response
                    .cast(&response)
                    .ok_or_else(|| Error::state_error(strand, "invalid response reference"))?
                    .enter(strand, async move |strand, response| {
                        // Borrow the response mutably to call chunk()
                        let mut response_borrow = response.borrow_mut(strand)?;
                        let inner = response_borrow
                            .inner
                            .as_mut()
                            .ok_or_else(|| Error::state_error(strand, "closed"))?;

                        match inner.chunk().await.into_http(strand)? {
                            Some(chunk) => {
                                Output::set(strand, out, chunk.as_ref());
                                Ok(true)
                            }
                            None => Ok(false),
                        }
                    })
                    .await
            })
            .await
    }
}

pub(crate) struct LineIter<'v> {
    buffer: BinEmbryo<'v>,
}

impl<'v> LineIter<'v> {
    fn new() -> Self {
        Self {
            buffer: BinEmbryo::new(),
        }
    }
}

impl<'v> Object<'v> for LineIter<'v> {
    const NAME: &'v str = "ResponseLineIter";
    const MODULE: &'v str = "http";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = *this.annex();

        loop {
            // Check if we have a complete line in the buffer
            {
                let mut borrow = this.borrow_mut(strand)?;
                if let Some((line, _)) = borrow.buffer.as_slice().split_once_str(b"\n") {
                    let line_len = line.len();
                    let trimmed_line_len = line_len - line.ends_with(b"\r") as usize;
                    let mut next =
                        BinEmbryo::new_with_capacity(strand, borrow.buffer.len() - (line_len + 1));
                    next.extend(strand, &borrow.buffer.as_slice()[line_len + 1..]);
                    borrow.buffer.truncate(trimmed_line_len);
                    let buf = mem::replace(&mut borrow.buffer, next);
                    drop(borrow);
                    buf.finish_str(strand, out)
                        .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                    return Ok(true);
                }
            }

            // Get the response object from slot 0 and read a chunk
            let chunk = strand
                .with_slots(async move |strand, [mut response]| {
                    let borrow = this.borrow(strand)?;
                    Output::set(strand, &mut response, Ref::slot::<0>(&borrow));
                    drop(borrow);
                    global
                        .types
                        .response
                        .cast(&response)
                        .ok_or_else(|| Error::state_error(strand, "invalid response reference"))?
                        .enter(strand, async move |strand, response| {
                            let mut response_borrow = response.borrow_mut(strand)?;
                            let inner = response_borrow
                                .inner
                                .as_mut()
                                .ok_or_else(|| Error::state_error(strand, "closed"))?;

                            inner.chunk().await.into_http(strand)
                        })
                        .await
                })
                .await?;

            match chunk {
                Some(chunk) => {
                    // Add to buffer
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.buffer.extend(strand, &chunk);
                }
                None => {
                    // End of stream - return any remaining data as the last line
                    let mut borrow = this.borrow_mut(strand)?;
                    if !borrow.buffer.is_empty() {
                        let buf = mem::take(&mut borrow.buffer);
                        drop(borrow);
                        buf.finish_str(strand, out)
                            .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
            }
        }
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let mut http = builder.module("http");

    http = http
        .value("Client", global.types.client)
        .value("Error", global.types.error)
        .value("Event", global.types.event)
        .value("Response", global.types.response)
        .value("Status", global.types.status);

    for method in global.http_methods.values() {
        let name = method.as_str().to_ascii_lowercase();
        let method = method.clone();
        http = http.function(&name, async move |strand, args, out| {
            request(
                &reqwest::Client::new(),
                global,
                strand,
                method.clone(),
                args,
                out,
            )
            .await
        })
    }

    http.commit();
}
