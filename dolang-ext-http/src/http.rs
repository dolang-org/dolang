use std::{
    borrow::Cow,
    fmt, mem,
    pin::Pin,
    result, str,
    task::{Context, Poll},
};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value,
    call,
    error::{ErrorKind, ResultExt as _},
    method,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::{Empty, TypeObject, View},
    vm::Builder,
};
use dolang_ext_shell::{as_datetime, datetime};
use reqwest::{
    Method,
    header::{HeaderMap, HeaderName, HeaderValue},
    tls::{Certificate, Identity},
};

use bstr::ByteSlice;
use bytes::Bytes;
use dolang_ext_url::{create_url, value_to_url};
use futures::stream::Stream;
use tokio::sync::mpsc;

use crate::{
    global::Global,
    sse::{EventIter, SseParser},
};

/// Custom error type for body streaming errors
#[derive(Debug)]
struct BodyError;

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request body stream error")
    }
}

impl std::error::Error for BodyError {}

/// Stream wrapper for tokio mpsc receiver
struct BodyStream(mpsc::Receiver<result::Result<Bytes, BodyError>>);

impl Stream for BodyStream {
    type Item = result::Result<Bytes, BodyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

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

fn output_headers<'v, 's>(
    strand: &mut Strand<'v, 's>,
    headers: &HeaderMap,
    mut out: Slot<'v, '_>,
    mut tmp: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    Output::set(strand, &mut out, Empty::Dict);
    let dict = out.as_dict(strand).unwrap();
    for (name, value) in headers {
        let value_str = header_value_to_str(value);
        if let Ok(time) = httpdate::parse_http_date(value_str.as_ref()) {
            datetime(strand, time, &mut tmp).map_err(|err| Error::runtime(strand, err))?;
            dict.insert(strand, name.as_str(), &tmp)?;
        } else {
            dict.insert(strand, name.as_str(), value_str.as_ref())?;
        }
    }
    Ok(())
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
    Error::type_error(strand, r#"status: expected :ignore: or "ignore""#)
}

fn parse_status_policy<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, StatusPolicy> {
    if value.as_sym(strand) == Some(global.syms.ignore) {
        Ok(StatusPolicy::Ignore)
    } else if let Some(str) = value.as_str(strand)
        && strand.access(|x| str.as_str(x) == "ignore")
    {
        Ok(StatusPolicy::Ignore)
    } else {
        Err(invalid_status_policy(strand))
    }
}

async fn status_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mut response: reqwest::Response,
) -> Error<'v, 's> {
    let status = response.status();
    let url = Some(response.url().clone());
    let headers = mem::take(response.headers_mut());
    let message = response
        .error_for_status_ref()
        .map(|_| status_message(status, url.as_ref()))
        .unwrap_or_else(|err| err.to_string());

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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().inner).map_err(|err| Error::runtime(strand, err))
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<http.Error ").map_err(|err| Error::runtime(strand, err))?;
        Self::display(this, strand, w)?;
        write!(w, ">").map_err(|err| Error::runtime(strand, err))
    }
}

impl<'v> Object<'v> for StatusObject {
    const NAME: &'v str = "Status";
    const MODULE: &'v str = "http";
    type Annex = StatusAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        #[cfg(feature = "json")]
        let from_str = builder.sym("from_str");

        #[cfg(feature = "json")]
        let builder =
            builder.method_with_slots("json", async move |this, strand, args, out, [mut json]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let text = status_text(strand, &annex)?;
                strand.import("json", &mut json).await?;
                method!(strand, json, from_str, out, text).await
            });

        builder
            .nominal_supertype(TypeObject::RuntimeError)
            .get("url", |this, strand, out| {
                output_url(strand, this.annex().url.as_ref(), out);
                Ok(())
            })
            .get("status", |this, strand, out| {
                Output::set(strand, out, i64::from(this.annex().status));
                Ok(())
            })
            .get_with_slots("headers", |this, strand, out, [mut tmp]| {
                output_headers(strand, &this.annex().headers, out, Slot::reborrow(&mut tmp))?;
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().message).map_err(|err| Error::runtime(strand, err))
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<http.Status ").map_err(|err| Error::runtime(strand, err))?;
        Self::display(this, strand, w)?;
        write!(w, ">").map_err(|err| Error::runtime(strand, err))
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

impl<T> ResultExt<T> for std::result::Result<T, reqwest::Error> {
    fn into_http<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T> {
        self.map_err(|error| error.into_http(strand))
    }
}

/// Pumps data from a VM iterator into a channel for request body streaming
async fn pump_request_body<'v, 's>(
    strand: &mut Strand<'v, 's>,
    iterator: &Value<'v>,
    sender: mpsc::Sender<result::Result<Bytes, BodyError>>,
    lines: bool,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut item]| {
            loop {
                match iterator.next(strand, &mut item).await {
                    Ok(true) => {
                        let mut vec = if let Some(slice) = item.as_bin(strand) {
                            slice.to_vec()
                        } else {
                            item.to_string(strand)?.into()
                        };

                        if lines {
                            vec.push(b'\n')
                        }

                        // Send to channel (with backpressure)
                        if sender.send(Ok(vec.into())).await.is_err() {
                            // Receiver dropped - request completed/cancelled
                            return Ok(());
                        }
                    }
                    Ok(false) => {
                        // Iterator exhausted - close channel
                        drop(sender);
                        return Ok(());
                    }
                    Err(e) => {
                        // Send dummy error to signal stream failure
                        // Real VM error propagates through pump_result
                        let _ = sender.send(Err(BodyError)).await;
                        return Err(e);
                    }
                }
            }
        })
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
            mut tmp,
        ]| {
            let mut url = None;
            let mut thunk = None;
            let mut body = None;
            let mut lines = None;
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
            if body.is_some() as usize + lines.is_some() as usize + json.is_some() as usize > 1 {
                return Err(Error::runtime(
                    st,
                    "at most one of `body`, `lines`, or `json` arguments may be specified",
                ));
            }
            #[cfg(not(feature = "json"))]
            if body.is_some() as usize + lines.is_some() as usize > 1 {
                return Err(Error::runtime(
                    st,
                    "at most one of `body` or `lines` arguments may be specified",
                ));
            }
            let url = url.ok_or_else(|| Error::missing_positional(st, 0))?;
            let url = value_to_url(st, &url)?;
            let mut builder = client.request(method, url);

            // Track streaming setup for later
            let mut stream = None;

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
                            // We have an iterator - set up streaming
                            let (sender, receiver) = mpsc::channel(8);
                            builder =
                                builder.body(reqwest::Body::wrap_stream(BodyStream(receiver)));

                            // Store sender for later pump creation
                            stream = Some((sender, false));
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
                let to_str = global.syms.to_str;
                method!(st, key, to_str, &mut value, json).await?;
                builder = builder
                    .body(value.to_string(st)?)
                    .header("content-type", "application/json");
            }

            if let Some(lines) = lines {
                match lines.iter(st, &mut body_iterator).await {
                    Ok(()) => {
                        let (sender, receiver) = mpsc::channel(8);
                        builder = builder.body(reqwest::Body::wrap_stream(BodyStream(receiver)));
                        stream = Some((sender, true));
                    }
                    Err(e) => return Err(e),
                }
            }

            if !queries.is_empty() {
                builder = builder.query(&queries);
            }
            builder = builder.headers(headers);

            // Execute request, running pump concurrently if streaming
            let response = if let Some((sender, lines)) = stream {
                // Create pump future now, after all other uses of st
                let pump = st.spawn_scoped(None, async move |strand| {
                    pump_request_body(strand, &body_iterator, sender, lines).await
                });

                // Wait for both to complete
                let (response_result, pump_result) = futures::join!(builder.send(), pump);

                pump_result?;

                // Then handle response
                response_result.into_http(st)?
            } else {
                // Non-streaming path
                builder.send().await.into_http(st)?
            };
            if status == StatusPolicy::Check && !response.status().is_success() {
                return Err(status_error(st, global, response).await);
            }
            let response = Response::new(response);
            global
                .types
                .response
                .create_with_annex(st, response, global, &mut value);
            let response = global.types.response.downcast(&value).unwrap();
            let mut borrow = response.borrow_mut(st).unwrap();
            output_headers(
                st,
                borrow.inner.as_ref().unwrap().headers(),
                Slot::reborrow(&mut iter),
                Slot::reborrow(&mut tmp),
            )?;
            Output::set(st, Mut::slot_mut::<0>(&mut borrow), iter);
            drop(borrow);
            if let Some(thunk) = thunk {
                let res = call!(st, thunk, out, &value).await;
                let _ = st
                    .with_interrupt_mask(true, async move |st| {
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
        let mut builder = reqwest::ClientBuilder::new();

        builder = if let Some(unix_socket) = unix_socket {
            #[cfg(unix)]
            {
                builder.unix_socket(
                    unix_socket
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "unix_socket: expected str"))?
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
                    .ok_or_else(|| Error::type_error(strand, "cookies: expected bool"))?,
            );
        }

        if let Some(ca_cert) = ca_cert {
            let cert = match ca_cert.view(strand) {
                View::Str(s) => strand.access(|x| Certificate::from_pem(s.as_str(x).as_bytes())),
                View::Bin(b) => strand.access(|x| Certificate::from_pem(b.as_slice(x))),
                _ => return Err(Error::type_error(strand, "ca_cert: expected str or bin")),
            }
            .into_http(strand)?;
            builder = builder.add_root_certificate(cert);
        }

        if let Some(identity) = identity {
            let id_bytes = identity
                .as_bin(strand)
                .ok_or_else(|| Error::type_error(strand, "identity: expected str or bin"))?;
            let pass = match password {
                Some(p) => p
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "password: expected str"))?
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

        if let Some(func) = func {
            strand
                .with_slots(async move |strand, [mut client, tmp]| {
                    let value = Client {
                        inner: Some(builder.build().into_http(strand)?),
                    };
                    this.create_with_annex(strand, value, ClientAnnex { global }, &mut client);
                    let res = call!(strand, func, out, &client).await;
                    let _ = strand
                        .with_interrupt_mask(true, async move |strand| {
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
    pub(crate) inner: Option<reqwest::Response>,
}

impl Response {
    fn new(inner: reqwest::Response) -> Self {
        Self { inner: Some(inner) }
    }
}

impl<'v> Object<'v> for Response {
    const NAME: &'v str = "Response";
    const MODULE: &'v str = "http";
    const SLOTS: usize = 1;
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

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        #[cfg(feature = "json")]
        let from_str = builder.sym("from_str");

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
                method!(strand, json, from_str, out, text.as_str()).await
            });

        builder
            .get("status", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                if borrow.inner.is_none() {
                    return Err(Error::state_error(strand, "closed"));
                }
                Output::set(
                    strand,
                    out,
                    borrow.inner.as_ref().unwrap().status().as_u16() as i64,
                );
                Ok(())
            })
            .get_with_slots("headers", move |this, strand, out, [mut tmp]| {
                let borrow = this.borrow(strand)?;
                if borrow.inner.is_none() {
                    return Err(Error::state_error(strand, "closed"));
                }
                output_headers(
                    strand,
                    borrow.inner.as_ref().unwrap().headers(),
                    out,
                    Slot::reborrow(&mut tmp),
                )?;
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
                Output::set(strand, out, res.as_ref());
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
                if inner.status().is_success() {
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
                    let iterator = global.types.chunk_iter.downcast(&iter).unwrap();
                    let mut iter_borrow = iterator.borrow_mut(strand)?;
                    Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                    drop(iter_borrow);
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
                    let iterator = global.types.event_iter.downcast(&iter).unwrap();
                    let mut iter_borrow = iterator.borrow_mut(strand)?;
                    Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                    drop(iter_borrow);
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
                let iterator = global.types.line_iter.downcast(&iter).unwrap();
                let mut iter_borrow = iterator.borrow_mut(strand)?;
                Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                drop(iter_borrow);
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

    async fn input<'a, 's>(
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
                let response = global
                    .types
                    .response
                    .downcast(&response)
                    .ok_or_else(|| Error::state_error(strand, "invalid response reference"))?;

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
    }
}

pub(crate) struct LineIter {
    buffer: Vec<u8>,
}

impl LineIter {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }
}

impl<'v> Object<'v> for LineIter {
    const NAME: &'v str = "ResponseLineIter";
    const MODULE: &'v str = "http";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
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
                if let Some((line, _)) = borrow.buffer.split_once_str(b"\n") {
                    let len = line.len() + 1;
                    let input = str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line))
                        .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                    Output::set(strand, out, input);
                    borrow.buffer.drain(..len);
                    return Ok(true);
                }
            }

            // Get the response object from slot 0 and read a chunk
            let chunk = strand
                .with_slots(async move |strand, [mut response]| {
                    let borrow = this.borrow(strand)?;
                    Output::set(strand, &mut response, Ref::slot::<0>(&borrow));
                    drop(borrow);
                    let response =
                        global.types.response.downcast(&response).ok_or_else(|| {
                            Error::state_error(strand, "invalid response reference")
                        })?;

                    let mut response_borrow = response.borrow_mut(strand)?;
                    let inner = response_borrow
                        .inner
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "closed"))?;

                    inner.chunk().await.into_http(strand)
                })
                .await?;

            match chunk {
                Some(chunk) => {
                    // Add to buffer
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.buffer.extend(chunk);
                }
                None => {
                    // End of stream - return any remaining data as the last line
                    let mut borrow = this.borrow_mut(strand)?;
                    if !borrow.buffer.is_empty() {
                        let input = str::from_utf8(&borrow.buffer)
                            .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                        Output::set(strand, out, input);
                        borrow.buffer.clear();
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
