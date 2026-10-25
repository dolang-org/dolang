use crate::global::Tag;

use std::{borrow::Cow, net::Ipv4Addr, net::SocketAddr, rc::Rc};

use bytes::Bytes;
use dolang::runtime::{
    Arg, Args, Error, Input, Object, Output, Result, Slot, State, Strand, Type, Value, call,
    io::IoResultExt as _,
    method,
    object::{DictLike, DictView, DictViewSink, Instance, Mut, Ref, TypeBuilder},
    strand::InterruptMask,
    unpack,
    value::{Array, Dict, Empty, TypeObject},
    vm::Register,
};
use dolang_ext_time::{as_datetime, datetime};
use dolang_ext_url::create_url;
use futures::{StreamExt as _, stream::FuturesUnordered};
use http::{HeaderName, HeaderValue, StatusCode};
use tokio::sync::{mpsc, oneshot};

use crate::global::Global;
use crate::serve::{self, Dispatch, RequestSnapshot, Response};

fn header_value_to_str(value: &HeaderValue) -> Cow<'_, str> {
    match value.to_str() {
        Ok(value) => Cow::Borrowed(value),
        Err(_) => String::from_utf8_lossy(value.as_bytes()),
    }
}

fn header_value_from_slot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
) -> Result<'v, 's, HeaderValue> {
    if let Some(time) = as_datetime(strand, value) {
        return HeaderValue::from_str(&httpdate::fmt_http_date(time))
            .map_err(|err| Error::runtime(strand, err));
    }
    HeaderValue::from_bytes(value.to_string(strand)?.as_bytes())
        .map_err(|err| Error::runtime(strand, err))
}

/// Writes `value` to `out`, auto-detecting HTTP-date-formatted values and
/// producing a `strand.DateTime` instead of a plain string for them — same
/// convention `dolang-ext-http`'s `Response.headers` uses.
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

/// Native `http.mock.Request` object: a request captured either by
/// `Mock.received()`/`Server.received_requests()` or handed to a
/// `match:`/`respond: do |req| ...` callback. `.headers` is a lazy
/// [`DictView`] over the underlying `HeaderMap` (see [`Headers`]) rather
/// than an eagerly-materialized dict — nothing here needs to allocate more
/// than once per request, even for a header queried many times or never at
/// all.
pub(crate) struct RequestObject;

impl<'v> Object<'v> for RequestObject {
    const NAME: &'v str = "Request";
    const MODULE: &'v str = "http.mock";
    type Annex = Rc<RequestSnapshot>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("method", |this, strand, out| {
                Output::set(strand, out, this.annex().method.as_str());
                Ok(())
            })
            .get("url", |this, strand, out| {
                Output::set(strand, out, this.annex().url.as_str());
                Ok(())
            })
            .get("body", |this, strand, out| {
                Output::set(strand, out, &this.annex().body[..]);
                Ok(())
            })
            .get("headers", |this, strand, out| {
                Output::set(strand, out, DictView::new(this, Headers));
                Ok(())
            })
    }
}

/// Lazy dict-like projection of a [`RequestObject`]'s headers. Header names
/// can repeat (e.g. multiple `Set-Cookie`-shaped headers), so `.get()`
/// supports the same `instance` selection as [`Dict::get`] instead of only
/// ever finding the first match.
///
/// [`Dict::get`]: dolang::runtime::value::Dict::get
struct Headers;

impl<'v> DictLike<'v> for Headers {
    type Object = RequestObject;
    const MODULE: &'v str = "http.mock";
    const NAME: &'v str = "Headers";

    fn len(&self, this: Instance<'v, '_, RequestObject>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().headers.iter().count()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, RequestObject>,
        strand: &'a mut Strand<'v, 's>,
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
        let annex = this.annex();
        let mut values = annex.headers.get_all(&name).iter();
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

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, RequestObject>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        for (name, value) in &annex.headers {
            strand.with_slots_sync(|strand, [mut tmp]| {
                header_value_or_datetime(strand, value, Slot::reborrow(&mut tmp))?;
                sink.push(strand, name.as_str(), &tmp);
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// Copies the array in `src` into a new array in `out`, so the caller gets a
/// stable view of a live array.
fn copy_array<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: &Value<'v>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut item]| {
        Output::set(strand, &mut out, Empty::Array);
        let dst = out.as_array(strand).unwrap();
        if let Some(src) = src.as_array(strand) {
            for i in 0..src.len(strand)? {
                src.get(strand, i, &mut item)?;
                dst.push(strand, &item)?;
            }
        }
        Ok(())
    })
}

/// One declarative matcher of a mock item.
enum Matcher {
    Method(http::Method),
    Path(String),
    PathRegex(regex::Regex),
    /// The header is present and every one of its values equals this one.
    /// Values aren't split on commas, which HTTP dates contain.
    Header(HeaderName, HeaderValue),
    /// Some decoded query pair equals this one.
    Query(String, String),
    #[cfg(feature = "json")]
    BodyJson(serde_json::Value),
}

impl Matcher {
    fn matches(&self, request: &RequestSnapshot) -> bool {
        match self {
            Matcher::Method(method) => request.method == *method,
            Matcher::Path(path) => request.url.path() == path,
            Matcher::PathRegex(regex) => regex.is_match(request.url.path()),
            Matcher::Header(name, value) => {
                let mut values = request.headers.get_all(name).iter().peekable();
                values.peek().is_some() && values.all(|v| v == value)
            }
            Matcher::Query(key, value) => request
                .url
                .query_pairs()
                .any(|(k, v)| k == key.as_str() && v == value.as_str()),
            #[cfg(feature = "json")]
            Matcher::BodyJson(json) => serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body == *json),
        }
    }
}

fn parse_expect<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
) -> Result<'v, 's, (u64, Option<u64>)> {
    if let Some(n) = value.as_int(strand) {
        let n =
            u64::try_from(n).map_err(|_| Error::value(strand, "expect must not be negative"))?;
        return Ok((n, Some(n)));
    }
    strand.with_slots_sync(|strand, [mut start, mut end, mut step]| {
        let range = value
            .as_range(strand)
            .ok_or_else(|| Error::type_error(strand, "expect must be an integer or a range"))?;
        range.parts(
            strand,
            [
                Slot::reborrow(&mut start),
                Slot::reborrow(&mut end),
                Slot::reborrow(&mut step),
            ],
        );
        if !step.is_nil() && step.as_int(strand) != Some(1) {
            return Err(Error::value(
                strand,
                "expect range step must be 1 or unspecified",
            ));
        }
        let min = if start.is_nil() {
            0
        } else {
            u64::try_from(start.as_int(strand).ok_or_else(|| {
                Error::type_error(strand, "expect range start must be an integer")
            })?)
            .map_err(|_| Error::value(strand, "expect range start must not be negative"))?
        };
        let max = if end.is_nil() {
            None
        } else {
            Some(
                u64::try_from(end.as_int(strand).ok_or_else(|| {
                    Error::type_error(strand, "expect range end must be an integer")
                })?)
                .map_err(|_| Error::value(strand, "expect range end must not be negative"))?,
            )
        };
        if max.is_some_and(|max| max < min) {
            return Err(Error::value(
                strand,
                "expect range end must not precede its start",
            ));
        }
        Ok((min, max))
    })
}

fn format_expect_range(min: u64, max: Option<u64>) -> String {
    match max {
        Some(max) if max == min => format!("{min}"),
        Some(max) => format!("{min}..={max}"),
        None => format!("{min}.."),
    }
}

/// Returns `None` if satisfied (or no `expect:` was given), or a failure
/// message otherwise. Plain `String` rather than `Error` since callers
/// aggregate messages across every entry of a `Mock` object before deciding
/// whether to raise anything.
fn verify_expect(entry: &MockEntry) -> Option<String> {
    let (min, max) = entry.expect?;
    let n = entry.count;
    if n >= min && max.is_none_or(|max| n <= max) {
        None
    } else {
        let label = entry.name.as_deref().unwrap_or("mock");
        Some(format!(
            "{label}: expected {} matching request(s), received {n}",
            format_expect_range(min, max)
        ))
    }
}

/// Checks every entry's `expect:`, raising one error listing every failure.
/// Entries of an unmounted `Mock` (by `.unmount()` or `server.reset()`) fail
/// their `expect:`.
fn verify_mock<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mock: Instance<'v, '_, MockObject>,
) -> Result<'v, 's, ()> {
    let messages = {
        let mock = mock.borrow(strand)?;
        let mut messages = Vec::new();
        for entry in &mock.entries {
            if entry.expect.is_none() {
                continue;
            }
            if !mock.mounted {
                let label = entry.name.as_deref().unwrap_or("mock");
                messages.push(format!("{label}: unmounted"));
            } else if let Some(msg) = verify_expect(entry) {
                messages.push(msg);
            }
        }
        messages
    };
    if messages.is_empty() {
        Ok(())
    } else {
        Err(Error::runtime(strand, messages.join("\n")))
    }
}

/// Combines two independent cleanup-step outcomes into one, chaining the
/// earlier (`prev`) error as the cause of the later (`next`) one when both
/// fail, rather than silently dropping either.
fn combine_results<'v, 's>(
    strand: &mut Strand<'v, 's>,
    prev: Result<'v, 's, ()>,
    next: Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    match (prev, next) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(cause), Err(error)) => Err(error.caused_by(strand, cause)),
    }
}

/// Parses one matcher/response spec dict (one dash item of `server.mock`'s
/// variadic argument list) into a [`MockEntry`].
///
/// Always pushes exactly one element to `callbacks` (nil if this spec used
/// no callback), keeping it 1:1 with the `Mock`'s entries; the dispatcher
/// finds an entry's `match:`/`respond:` closures there by index.
#[cfg_attr(not(feature = "json"), allow(unused_variables, unused_mut))]
async fn parse_mock_spec<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    spec: &Slot<'v, '_>,
    callbacks: &Array<'v, '_>,
) -> Result<'v, 's, MockEntry> {
    let dict = spec
        .as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, "mock: expected a Dict"))?;

    let (matchers, response, has_respond, expect, name) = strand
        .with_slots(
            async move |strand,
                        [
                mut tmp,
                mut tmp2,
                mut tmp3,
                mut match_cb,
                mut respond_cb,
                mut record,
            ]| {
                let mut matchers = Vec::new();
                let mut response = None;
                let mut has_respond = false;
                let mut expect: Option<(u64, Option<u64>)> = None;
                let mut name: Option<String> = None;
                let mut uses_callback = false;

                if dict.get(strand, global.syms.method, None, &mut tmp)? {
                    let method = tmp.to_string(strand)?.to_ascii_uppercase();
                    let method = http::Method::from_bytes(method.as_bytes()).map_err(|_| {
                        Error::value(strand, format!("method: invalid HTTP method {method:?}"))
                    })?;
                    matchers.push(Matcher::Method(method));
                }
                if dict.get(strand, global.syms.path, None, &mut tmp)? {
                    let path = tmp.to_string(strand)?;
                    if path.contains('?') {
                        return Err(Error::value(
                            strand,
                            format!(
                                "path: {path:?} contains `?`; match query parameters with `query:`"
                            ),
                        ));
                    }
                    if url::Url::parse(&path).is_ok_and(|url| url.host_str().is_some()) {
                        return Err(Error::value(
                            strand,
                            format!("path: {path:?} must not include a host"),
                        ));
                    }
                    let path = if path.starts_with('/') {
                        path
                    } else {
                        format!("/{path}")
                    };
                    matchers.push(Matcher::Path(path));
                }
                if dict.get(strand, global.syms.path_regex, None, &mut tmp)? {
                    let pattern = tmp.to_string(strand)?;
                    let regex = regex::Regex::new(&pattern)
                        .map_err(|err| Error::value(strand, format!("path_regex: {err}")))?;
                    matchers.push(Matcher::PathRegex(regex));
                }
                if dict.get(strand, global.syms.headers, None, &mut tmp)? {
                    let headers_dict = tmp
                        .as_dict(strand)
                        .ok_or_else(|| Error::type_error(strand, "headers must be a dict"))?;
                    dict_for_each(
                        strand,
                        &headers_dict,
                        Slot::reborrow(&mut tmp2),
                        Slot::reborrow(&mut tmp3),
                        |strand, key, value| {
                            let name = HeaderName::from_bytes(key.to_string(strand)?.as_bytes())
                                .map_err(|err| Error::runtime(strand, err))?;
                            let hvalue = header_value_from_slot(strand, value)?;
                            matchers.push(Matcher::Header(name, hvalue));
                            Ok(())
                        },
                    )?;
                }
                if dict.get(strand, global.syms.query, None, &mut tmp)? {
                    let query_dict = tmp
                        .as_dict(strand)
                        .ok_or_else(|| Error::type_error(strand, "query must be a dict"))?;
                    dict_for_each(
                        strand,
                        &query_dict,
                        Slot::reborrow(&mut tmp2),
                        Slot::reborrow(&mut tmp3),
                        |strand, key, value| {
                            matchers.push(Matcher::Query(
                                key.to_string(strand)?,
                                value.to_string(strand)?,
                            ));
                            Ok(())
                        },
                    )?;
                }
                #[cfg(feature = "json")]
                if dict.get(strand, global.syms.body_json, None, &mut tmp)? {
                    strand.import("json", &mut tmp2).await?;
                    let encode = global.syms.encode;
                    method!(strand, Slot::reborrow(&mut tmp2), encode, &mut tmp3, &tmp).await?;
                    let json = serde_json::from_str(&tmp3.to_string(strand)?)
                        .map_err(|err| Error::runtime(strand, err))?;
                    matchers.push(Matcher::BodyJson(json));
                }
                #[cfg(not(feature = "json"))]
                if dict.get(strand, global.syms.body_json, None, &mut tmp)? {
                    return Err(Error::runtime(
                        strand,
                        "body_json requires the json feature",
                    ));
                }
                if dict.get(strand, global.syms.match_kw, None, &mut match_cb)? {
                    uses_callback = true;
                }
                if dict.get(strand, global.syms.expect, None, &mut tmp)? {
                    expect = Some(parse_expect(strand, &tmp)?);
                }
                if dict.get(strand, global.syms.name, None, &mut tmp)? {
                    name = Some(tmp.to_string(strand)?);
                }

                if dict.get(strand, global.syms.respond, None, &mut respond_cb)? {
                    has_respond = true;
                    if respond_cb.as_dict(strand).is_some() {
                        response = Some(build_response(strand, global, &respond_cb).await?);
                    } else {
                        uses_callback = true;
                    }
                }

                if uses_callback {
                    Output::set(strand, &mut record, Empty::Dict);
                    let record_dict = record.as_dict(strand).unwrap();
                    record_dict.insert(strand, global.syms.match_kw, &match_cb, false)?;
                    record_dict.insert(strand, global.syms.respond, &respond_cb, false)?;
                }
                callbacks.push(strand, &record)?;

                Ok((matchers, response, has_respond, expect, name))
            },
        )
        .await?;

    if !has_respond {
        return Err(Error::missing_key(strand, "respond"));
    }
    Ok(MockEntry {
        matchers,
        response,
        expect,
        name,
        count: 0,
    })
}

/// Walks a dict's pairs, calling `f` with (key, value) scratch slots reborrowed
/// per pair (`key`/`value` are freely reusable scratch slots owned by the caller).
fn dict_for_each<'v, 's>(
    strand: &mut Strand<'v, 's>,
    dict: &Dict<'v, '_>,
    mut key: Slot<'v, '_>,
    mut value: Slot<'v, '_>,
    mut f: impl FnMut(&mut Strand<'v, 's>, &Slot<'v, '_>, &Slot<'v, '_>) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    let mut pairs = dict.pairs();
    while pairs.next(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut value))? {
        f(strand, &key, &value)?;
    }
    Ok(())
}

#[cfg_attr(not(feature = "json"), allow(unused_variables, unused_mut))]
async fn build_response<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    respond: &Slot<'v, '_>,
) -> Result<'v, 's, Response> {
    let dict = respond
        .as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, "respond must be a dict"))?;

    let (status, headers, body) = strand
        .with_slots(async move |strand, [mut tmp, mut tmp2, mut tmp3]| {
            let mut status: Option<u16> = None;
            let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
            let mut body: Option<Vec<u8>> = None;

            if dict.get(strand, global.syms.status, None, &mut tmp)? {
                let n = tmp.as_int(strand).ok_or_else(|| {
                    Error::type_error(strand, "respond.status must be an integer")
                })?;
                status = Some(
                    u16::try_from(n)
                        .map_err(|_| Error::value(strand, "respond.status out of range"))?,
                );
            }

            if dict.get(strand, global.syms.headers, None, &mut tmp)? {
                let headers_dict = tmp
                    .as_dict(strand)
                    .ok_or_else(|| Error::type_error(strand, "respond.headers must be a dict"))?;
                dict_for_each(
                    strand,
                    &headers_dict,
                    Slot::reborrow(&mut tmp2),
                    Slot::reborrow(&mut tmp3),
                    |strand, key, value| {
                        let name = HeaderName::from_bytes(key.to_string(strand)?.as_bytes())
                            .map_err(|err| Error::runtime(strand, err))?;
                        let hvalue = header_value_from_slot(strand, value)?;
                        headers.push((name, hvalue));
                        Ok(())
                    },
                )?;
            }

            if dict.get(strand, global.syms.body, None, &mut tmp)? {
                if let Some(slice) = tmp.as_bin(strand) {
                    body = Some(slice.to_vec());
                } else {
                    body = Some(tmp.to_string(strand)?.into_bytes());
                }
            }

            #[cfg(feature = "json")]
            if dict.get(strand, global.syms.json, None, &mut tmp)? {
                strand.import("json", &mut tmp2).await?;
                let encode = global.syms.encode;
                method!(strand, Slot::reborrow(&mut tmp2), encode, &mut tmp3, &tmp).await?;
                body = Some(tmp3.to_string(strand)?.into_bytes());
                headers.push((
                    HeaderName::from_static("content-type"),
                    HeaderValue::from_static("application/json"),
                ));
            }

            Ok((status, headers, body))
        })
        .await?;

    let status = StatusCode::from_u16(status.unwrap_or(200))
        .map_err(|_| Error::value(strand, "respond.status out of range"))?;
    Ok(Response {
        status,
        headers,
        body: body.map(Bytes::from).unwrap_or_default(),
    })
}

/// Runs a server's dispatcher strand: each request from [`serve::serve`]
/// is answered in its own scoped child strand, so a slow callback (or one
/// that calls the same server) doesn't hold up other requests. Exits when the
/// server task drops its sender, or when cancelled by `close`.
async fn run_dispatcher<'v, 's>(
    strand: &mut Strand<'v, 's>,
    server: &Value<'v>,
    mut rx: mpsc::UnboundedReceiver<Dispatch>,
) -> Result<'v, 's, ()> {
    let strand = &*strand;
    let mut in_flight = FuturesUnordered::new();
    loop {
        tokio::select! {
            dispatch = rx.recv() => {
                let Some(Dispatch { request, reply }) = dispatch else {
                    break;
                };
                in_flight.push(strand.spawn_scoped(None, async move |strand| {
                    let response = match respond(strand, server, request).await {
                        Ok(response) => response,
                        Err(err) => Response::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("http.mock: {err}"),
                        ),
                    };
                    let _ = reply.send(response);
                    Ok(())
                }));
            }
            Some(_) = in_flight.next(), if !in_flight.is_empty() => {}
        }
    }
    Ok(())
}

/// Records `request` on `server` and answers it from the first matching
/// mounted mock, or with a 404.
async fn respond<'v, 's>(
    strand: &mut Strand<'v, 's>,
    server: &Value<'v>,
    request: RequestSnapshot,
) -> Result<'v, 's, Response> {
    let global = strand.state::<Tag>();
    let request = Rc::new(request);
    strand
        .with_slots(
            async |strand, [mut req, mut mounted, mut received, mut mock]| {
                global.types.request.create_with_annex(
                    strand,
                    RequestObject,
                    request.clone(),
                    &mut req,
                );
                global.types.server.cast(server).unwrap().enter_sync(
                    strand,
                    |strand, server| {
                        let borrow = server.borrow(strand)?;
                        // Mocks can unmount while a callback runs, so walk a copy.
                        copy_array(
                            strand,
                            Ref::slot::<0>(&borrow),
                            Slot::reborrow(&mut mounted),
                        )?;
                        Output::set(strand, &mut received, Ref::slot::<2>(&borrow));
                        Ok(())
                    },
                )?;
                if let Some(received) = received.as_array(strand) {
                    received.push(strand, &req)?;
                }
                let mounted = mounted.as_array(strand).unwrap();
                for i in 0..mounted.len(strand)? {
                    mounted.get(strand, i, &mut mock)?;
                    let response = global
                        .types
                        .mock
                        .cast(&mock)
                        .unwrap()
                        .enter(strand, async |strand, mock| {
                            respond_mock(strand, global, mock, &request, &req).await
                        })
                        .await?;
                    if let Some(response) = response {
                        return Ok(response);
                    }
                }
                Ok(Response::new(StatusCode::NOT_FOUND, Bytes::new()))
            },
        )
        .await
}

/// Answers `request` from the first of `mock`'s entries that matches it, if
/// any, counting and recording it there.
async fn respond_mock<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mock: Instance<'v, '_, MockObject>,
    request: &RequestSnapshot,
    req: &Slot<'v, '_>,
) -> Result<'v, 's, Option<Response>> {
    strand
        .with_slots(
            async |strand, [mut callbacks, mut received, mut record, mut cb, mut ret]| {
                let len = {
                    let borrow = mock.borrow(strand)?;
                    Output::set(strand, &mut callbacks, Ref::slot::<1>(&borrow));
                    borrow.entries.len()
                };
                let callbacks = callbacks.as_array(strand).unwrap();
                for i in 0..len {
                    {
                        let borrow = mock.borrow(strand)?;
                        if !borrow.mounted {
                            return Ok(None);
                        }
                        if !borrow.entries[i]
                            .matchers
                            .iter()
                            .all(|m| m.matches(request))
                        {
                            continue;
                        }
                    }
                    callbacks.get(strand, i, &mut record)?;
                    let record = record.as_dict(strand);
                    if let Some(record) = &record
                        && record.get(strand, global.syms.match_kw, None, &mut cb)?
                        && !cb.is_nil()
                    {
                        // A failing `match:` counts as no match.
                        let matched = match call!(strand, &cb, &mut ret, req).await {
                            Ok(()) => ret.to_bool(strand).unwrap_or(false),
                            Err(_) => false,
                        };
                        if !matched {
                            continue;
                        }
                    }

                    let response = {
                        let mut borrow = mock.borrow_mut(strand)?;
                        if !borrow.mounted {
                            return Ok(None);
                        }
                        borrow.entries[i].count += 1;
                        Output::set(strand, &mut received, Mut::slot::<2>(&borrow));
                        borrow.entries[i].response.clone()
                    };
                    if let Some(received) = received.as_array(strand) {
                        received.push(strand, req)?;
                    }
                    let response = match response {
                        Some(response) => response,
                        None => {
                            record
                                .unwrap()
                                .get(strand, global.syms.respond, None, &mut cb)?;
                            call!(strand, &cb, &mut ret, req).await?;
                            build_response(strand, global, &ret).await?
                        }
                    };
                    return Ok(Some(response));
                }
                Ok(None)
            },
        )
        .await
}

/// Slot 0 holds a Do array of every mounted `Mock` this server has
/// registered, scoped or not, in registration order: the dispatcher matches
/// requests against it, and it keeps a persistent mock reachable whether or
/// not the script retained the handle `.mock()` returned. Unmounting a `Mock`
/// removes its entry.
///
/// Slot 1 holds the `strand.Strand` handle of the dispatcher strand (see
/// [`run_dispatcher`]), which `close` cancels.
///
/// Slot 2 holds a Do array of every request received, matched or not.
pub(crate) struct Server {
    address: SocketAddr,
    /// Dropping this stops the server task.
    shutdown: Option<oneshot::Sender<()>>,
    /// Resolves once the server task has dropped its listener.
    stopped: Option<oneshot::Receiver<()>>,
}

impl Server {
    fn check_open<'v, 's>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        if self.shutdown.is_none() {
            return Err(Error::state_error(strand, "server closed"));
        }
        Ok(())
    }
}

pub(crate) struct ServerAnnex<'v> {
    global: State<'v, Global<'v>>,
}

struct MockEntry {
    matchers: Vec<Matcher>,
    /// `None` when `respond:` is a callback.
    response: Option<Response>,
    expect: Option<(u64, Option<u64>)>,
    name: Option<String>,
    /// Requests this entry has matched.
    count: u64,
}

/// Slot 0 holds the owning `Server`, so unmounting can remove the `Mock`
/// from that server's array of mounted mocks (see `Server`'s slots) without
/// any Rc/RefCell bookkeeping — liveness is just "reachable in the object
/// graph", the same way the GC already tracks everything else.
///
/// Slot 1 holds a Do array with one element per entry: a `{match:, respond:}`
/// dict of the entry's callbacks, or nil if it has none.
///
/// Slot 2 holds a Do array of the requests this `Mock` matched, in arrival
/// order.
pub(crate) struct MockObject {
    entries: Vec<MockEntry>,
    mounted: bool,
}

pub(crate) type MockObjectAnnex<'v> = State<'v, Global<'v>>;

/// Binds a listener, starts the server task and the dispatcher strand, and
/// writes the new `Server` to `out`.
async fn start_server<'v, 's>(
    this: Type<'v, Server>,
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(Error::runtime(strand, "http.mock requires a tokio runtime"));
    }
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .into_do_io(strand)?;
    let address = listener.local_addr().into_do_io(strand)?;
    let (dispatch_tx, dispatch_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (stopped_tx, stopped_rx) = oneshot::channel();
    let value = Server {
        address,
        shutdown: Some(shutdown_tx),
        stopped: Some(stopped_rx),
    };
    this.create_with_annex(
        strand,
        value,
        ServerAnnex { global },
        Slot::reborrow(&mut out),
    );
    strand
        .with_slots(async |strand, [mut dispatcher]| {
            // The dispatcher roots the `Server`, and the `Server` roots the
            // dispatcher's handle; if neither is otherwise reachable, the
            // collector unwinds the strand and drops the `Server`, which stops
            // the server task.
            strand.spawn_background(
                &out,
                None,
                &mut dispatcher,
                async move |strand, server, _out| run_dispatcher(strand, server, dispatch_rx).await,
            )?;
            this.cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, instance| {
                    let mut borrow = instance.borrow_mut(strand)?;
                    Output::set(strand, Mut::slot_mut::<0>(&mut borrow), Empty::Array);
                    Output::set(strand, Mut::slot_mut::<1>(&mut borrow), &dispatcher);
                    Output::set(strand, Mut::slot_mut::<2>(&mut borrow), Empty::Array);
                    Ok(())
                })
        })
        .await?;
    strand
        .vm()
        .spawn_task(serve::serve(listener, dispatch_tx, shutdown_rx, stopped_tx));
    Ok(())
}

impl<'v> Object<'v> for Server {
    const NAME: &'v str = "Server";
    const MODULE: &'v str = "http.mock";
    const SLOTS: usize = 3;
    type Annex = ServerAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        let ([], [func]) = unpack!(strand, args, 0, 1)?;

        if let Some(func) = func {
            strand
                .with_slots(async move |strand, [mut handle, tmp]| {
                    start_server(this, strand, global, Slot::reborrow(&mut handle)).await?;
                    let res = call!(strand, func, out, &handle).await;
                    let _ = strand
                        .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                            method!(strand, handle, global.syms.close, tmp).await
                        })
                        .await;
                    res
                })
                .await
        } else {
            start_server(this, strand, global, Slot::reborrow(&mut out)).await
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Func)
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = this.annex().global;
                let stopped = {
                    let mut borrow = this.borrow_mut(strand)?;
                    drop(borrow.shutdown.take());
                    borrow.stopped.take()
                };
                strand
                    .with_slots(async |strand, [mut dispatcher, mut tmp]| {
                        {
                            let borrow = this.borrow(strand)?;
                            Output::set(strand, &mut dispatcher, Ref::slot::<1>(&borrow));
                        }
                        if !dispatcher.is_nil() {
                            method!(
                                strand,
                                Slot::reborrow(&mut dispatcher),
                                global.syms.cancel,
                                &mut tmp
                            )
                            .await?;
                        }
                        Ok(())
                    })
                    .await?;
                if let Some(stopped) = stopped {
                    let _ = stopped.await;
                }
                Ok(())
            })
            .method("reset", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = this.annex().global;
                this.borrow(strand)?.check_open(strand)?;
                strand.with_slots_sync(|strand, [mut mounted, mut item]| {
                    {
                        let mut borrow = this.borrow_mut(strand)?;
                        Output::set(strand, &mut mounted, Mut::slot::<0>(&borrow));
                        Output::set(strand, Mut::slot_mut::<0>(&mut borrow), Empty::Array);
                        Output::set(strand, Mut::slot_mut::<2>(&mut borrow), Empty::Array);
                    }
                    let mut result = Ok(());
                    if let Some(mounted) = mounted.as_array(strand) {
                        for i in 0..mounted.len(strand)? {
                            mounted.get(strand, i, &mut item)?;
                            let next = global
                                .types
                                .mock
                                .cast(&item)
                                .unwrap()
                                .enter_sync(strand, |strand, instance| {
                                    unmount_mock(strand, instance, global)
                                });
                            result = combine_results(strand, result, next);
                        }
                    }
                    result
                })
            })
            .method("received_requests", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                borrow.check_open(strand)?;
                copy_array(strand, Ref::slot::<2>(&borrow), out)
            })
            .get("address", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                borrow.check_open(strand)?;
                Output::set(strand, out, borrow.address.to_string().as_str());
                Ok(())
            })
            .get("url", |this, strand, out| {
                let address = {
                    let borrow = this.borrow(strand)?;
                    borrow.check_open(strand)?;
                    borrow.address
                };
                let url = url::Url::parse(&format!("http://{address}"))
                    .map_err(|err| Error::runtime(strand, err))?;
                create_url(strand, url, out);
                Ok(())
            })
            .method("mock", async move |this, strand, args, out| {
                let global = this.annex().global;
                strand
                    .with_slots(async move |strand, [mut item, mut callbacks]| {
                        let mut specs: Vec<Slot<'_, '_>> = Vec::new();
                        let mut block = None;

                        for arg in args {
                            match arg {
                                Arg::Pos(slot) => {
                                    if slot.as_dict(strand).is_some() {
                                        specs.push(slot);
                                    } else if block.is_none() {
                                        block = Some(slot);
                                    } else {
                                        return Err(Error::unexpected_positional(
                                            strand,
                                            specs.len() + 1,
                                        ));
                                    }
                                }
                                Arg::Key(sym, _) => {
                                    return Err(Error::unexpected_key(strand, sym));
                                }
                            }
                        }
                        if specs.is_empty() {
                            return Err(Error::runtime(
                                strand,
                                "mock: at least one matcher/response dict is required",
                            ));
                        }
                        this.borrow(strand)?.check_open(strand)?;

                        Output::set(strand, &mut callbacks, Empty::Array);
                        let callbacks_array = callbacks.as_array(strand).unwrap();
                        let mut entries = Vec::new();
                        for spec in &specs {
                            entries.push(
                                parse_mock_spec(strand, global, spec, &callbacks_array).await?,
                            );
                        }

                        // Register this `Mock` in the server's array of
                        // mounted mocks, and give it a back-reference to the
                        // server so unmounting can remove it from that array.
                        let mock_object = MockObject {
                            entries,
                            mounted: true,
                        };
                        global
                            .types
                            .mock
                            .create_with_annex(strand, mock_object, global, &mut item);
                        global.types.mock.cast(&item).unwrap().enter_sync(
                            strand,
                            |strand, instance| {
                                let mut mock_borrow = instance.borrow_mut(strand)?;
                                Output::set(strand, Mut::slot_mut::<0>(&mut mock_borrow), this);
                                Output::set(
                                    strand,
                                    Mut::slot_mut::<1>(&mut mock_borrow),
                                    &callbacks,
                                );
                                Output::set(
                                    strand,
                                    Mut::slot_mut::<2>(&mut mock_borrow),
                                    Empty::Array,
                                );
                                Ok(())
                            },
                        )?;
                        {
                            let server_borrow = this.borrow(strand)?;
                            let array = Ref::slot::<0>(&server_borrow).as_array(strand).unwrap();
                            array.push(strand, &item)?;
                        }

                        if let Some(block) = block {
                            let result = call!(strand, block, out, &item).await;
                            let cleanup = strand
                                .with_interrupt_mask(InterruptMask::all(), async |strand| {
                                    global.types.mock.cast(&item).unwrap().enter_sync(
                                        strand,
                                        |strand, instance| {
                                            let verified = verify_mock(strand, instance);
                                            let unmounted = unmount_mock(strand, instance, global);
                                            combine_results(strand, verified, unmounted)
                                        },
                                    )
                                })
                                .await;
                            combine_results(strand, result, cleanup)
                        } else {
                            Output::set(strand, out, &item);
                            Ok(())
                        }
                    })
                    .await
            })
    }
}

/// Unmounts a `Mock`, forgets the requests it matched, and removes it from
/// its server's array of mounted mocks. Shared by `.unmount()`, scoped
/// `server.mock ... do |mock| ...` cleanup, and `server.reset()`.
fn unmount_mock<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, MockObject>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut server]| {
        {
            let mut borrow = this.borrow_mut(strand)?;
            borrow.mounted = false;
            Output::set(strand, Mut::slot_mut::<2>(&mut borrow), Empty::Array);
            Output::set(strand, &mut server, Mut::slot::<0>(&borrow));
        }
        if let Some(server) = global.types.server.cast(&server) {
            server.enter_sync(strand, |strand, server| {
                let server_borrow = server.borrow(strand)?;
                if let Some(array) = Ref::slot::<0>(&server_borrow).as_array(strand) {
                    remove_identical(strand, &array, this)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}

/// Removes the array element identical to `target` (by identity), if
/// present.
fn remove_identical<'v, 's>(
    strand: &mut Strand<'v, 's>,
    array: &Array<'v, '_>,
    target: impl Input<'v> + Copy,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut tmp]| {
        let len = array.len(strand)?;
        for i in 0..len {
            array.get(strand, i, &mut tmp)?;
            if tmp.eq(strand, target)? {
                array.delete(strand, i)?;
                break;
            }
        }
        Ok(())
    })
}

impl<'v> Object<'v> for MockObject {
    const NAME: &'v str = "Mock";
    const MODULE: &'v str = "http.mock";
    const SLOTS: usize = 3;
    type Annex = MockObjectAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("received", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                copy_array(strand, Ref::slot::<2>(&borrow), out)
            })
            .method("verify", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                verify_mock(strand, this)
            })
            .method("unmount", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = *this.annex();
                unmount_mock(strand, this, global)
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("http.mock")
        .value("Mock", global.types.mock)
        .value("Request", global.types.request)
        .value("Server", global.types.server)
        .commit();
}
