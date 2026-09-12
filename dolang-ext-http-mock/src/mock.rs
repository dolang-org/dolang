use std::borrow::Cow;

use dolang::runtime::{
    Arg, Args, Error, Input, Object, Output, Result, Slot, State, Strand, Type, Value, call,
    error::ResultExt as _,
    method,
    object::{DictLike, DictView, DictViewSink, Instance, Mut, Ref, TypeBuilder},
    strand::InterruptMask,
    unpack,
    value::{Array, Dict, Empty},
    vm::Builder,
};
use dolang_ext_time::{as_datetime, datetime};
use dolang_ext_url::create_url;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use wiremock::{Match, Mock, MockGuard, MockServer, Request, Respond, ResponseTemplate, matchers};

use crate::global::Global;

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

/// Owned, `Send + 'static` snapshot of a [`Request`], built on wiremock's
/// dedicated runtime thread (inside a synchronous `Match`/`Respond`
/// callback) and shipped across to the main VM thread's background strand.
/// Doubles as [`RequestObject`]'s `Annex` — none of these fields ever change
/// after capture, so there's no need for a separate GC-facing struct.
pub(crate) struct RequestSnapshot {
    method: String,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl RequestSnapshot {
    fn capture(request: &Request) -> Self {
        Self {
            method: request.method.as_str().to_string(),
            url: request.url.to_string(),
            headers: request.headers.clone(),
            body: request.body.clone(),
        }
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
    type Annex = RequestSnapshot;
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
                Output::set(strand, out, this.annex().body.as_slice());
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

/// Builds a [`RequestObject`] from a captured `wiremock::Request`.
fn create_request<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    request: &Request,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    global.types.request.create_with_annex(
        strand,
        RequestObject,
        RequestSnapshot::capture(request),
        out,
    );
    Ok(())
}

/// Builds a [`RequestObject`] from an already-captured [`RequestSnapshot`]
/// (the cross-thread payload used by `match:`/`respond:` callbacks — see
/// [`Dispatch`]).
fn create_request_from_snapshot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    snapshot: RequestSnapshot,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    global
        .types
        .request
        .create_with_annex(strand, RequestObject, snapshot, out);
    Ok(())
}

fn requests_to_array<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    requests: &[Request],
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut item]| {
        Output::set(strand, &mut out, Empty::Array);
        let array = out.as_array(strand).unwrap();
        for req in requests {
            create_request(strand, global, req, &mut item)?;
            array.push(strand, &item)?;
        }
        Ok(())
    })
}

/// One message sent from a synchronous wiremock `Match`/`Respond` callback
/// (running on `Server`'s dedicated tokio runtime thread, see
/// [`spawn_runtime`]) to the `Mock`'s background strand (running on the main
/// VM thread, see [`run_callback_strand`]). `index` identifies which spec's
/// `match:`/`respond:` closure to invoke, matching its position in the
/// `Mock`'s callback-records array (see [`MockObject`]'s slot 1).
enum Dispatch {
    Match {
        index: usize,
        request: RequestSnapshot,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    Respond {
        index: usize,
        request: RequestSnapshot,
        reply: tokio::sync::oneshot::Sender<ResponseTemplate>,
    },
}

/// Bridges a wiremock `Match` callback to a `match: do |req| ...` Do
/// closure. `matches` runs synchronously on `Server`'s dedicated runtime
/// thread; it ships a [`Dispatch::Match`] over `tx` and blocks (via
/// `futures::executor::block_on`, safe here since nothing on this thread's
/// single-threaded runtime needs to keep running while we wait — the reply
/// is produced entirely by the other thread's VM event loop) until the
/// background strand replies.
struct CallbackMatch {
    index: usize,
    tx: tokio::sync::mpsc::UnboundedSender<Dispatch>,
}

impl Match for CallbackMatch {
    fn matches(&self, request: &Request) -> bool {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let msg = Dispatch::Match {
            index: self.index,
            request: RequestSnapshot::capture(request),
            reply,
        };
        if self.tx.send(msg).is_err() {
            return false;
        }
        futures::executor::block_on(rx).unwrap_or(false)
    }
}

/// Bridges a wiremock `Respond` callback to a `respond: do |req| ...` Do
/// closure. Same blocking-bridge approach as [`CallbackMatch`]. If the
/// background strand (or the closure it invokes) fails — including the Do
/// closure raising an error — a `500` response carrying the failure message
/// is returned rather than propagating a panic across the FFI-like
/// sync/async boundary.
struct CallbackRespond {
    index: usize,
    tx: tokio::sync::mpsc::UnboundedSender<Dispatch>,
}

impl Respond for CallbackRespond {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let msg = Dispatch::Respond {
            index: self.index,
            request: RequestSnapshot::capture(request),
            reply,
        };
        if self.tx.send(msg).is_err() {
            return ResponseTemplate::new(500)
                .set_body_string("http.mock: callback strand is gone");
        }
        futures::executor::block_on(rx).unwrap_or_else(|_| {
            ResponseTemplate::new(500).set_body_string("http.mock: callback strand is gone")
        })
    }
}

/// Either a static response template or a callback-backed one; both
/// implement `Respond` so `Mock::respond_with` can take either uniformly.
enum Responder {
    Static(ResponseTemplate),
    Callback(CallbackRespond),
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match self {
            Responder::Static(template) => template.respond(request),
            Responder::Callback(callback) => callback.respond(request),
        }
    }
}

/// Runs the single background strand backing one `Mock` object's
/// callback-based matchers/responders (see [`MockObject`]'s slot 2).
/// Receives [`Dispatch`] messages from any of the `Mock`'s registered
/// `CallbackMatch`/`CallbackRespond` instances, looks up the corresponding
/// entry in `callbacks` (a Do array of `{match: callable?, respond:
/// callable?}` dicts, one per spec, indexed the same as `MockEntry`), invokes
/// the closure, and replies. Exits once every sender is dropped (i.e. once
/// the `Mock` unmounts all of its entries), or the strand is cancelled.
async fn run_callback_strand<'v, 's>(
    strand: &mut Strand<'v, 's>,
    callbacks: &Value<'v>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Dispatch>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let callbacks = callbacks.as_array(strand).unwrap();
    while let Some(msg) = rx.recv().await {
        match msg {
            Dispatch::Match {
                index,
                request,
                reply,
            } => {
                let matched = strand
                    .with_slots(
                        async |strand,
                               [mut record, mut cb, mut req, mut ret]|
                               -> Result<'v, 's, bool> {
                            callbacks.get(strand, index, &mut record)?;
                            let record_dict = record.as_dict(strand).unwrap();
                            if !record_dict.get(strand, global.syms.match_kw, None, &mut cb)? {
                                return Ok(true);
                            }
                            create_request_from_snapshot(
                                strand,
                                global,
                                request,
                                Slot::reborrow(&mut req),
                            )?;
                            call!(strand, &cb, &mut ret, &req).await?;
                            Ok(ret.to_bool(strand))
                        },
                    )
                    .await
                    .unwrap_or(false);
                let _ = reply.send(matched);
            }
            Dispatch::Respond {
                index,
                request,
                reply,
            } => {
                let response = strand
                    .with_slots(async |strand, [mut record, mut cb, mut req, mut ret]| {
                        callbacks.get(strand, index, &mut record)?;
                        let record_dict = record.as_dict(strand).unwrap();
                        record_dict.get(strand, global.syms.respond, None, &mut cb)?;
                        create_request_from_snapshot(
                            strand,
                            global,
                            request,
                            Slot::reborrow(&mut req),
                        )?;
                        call!(strand, &cb, &mut ret, &req).await?;
                        build_response(strand, global, &ret).await
                    })
                    .await
                    .unwrap_or_else(|err| {
                        ResponseTemplate::new(500).set_body_string(format!("http.mock: {err}"))
                    });
                let _ = reply.send(response);
            }
        }
    }
    Ok(())
}

/// Matcher accumulator: matches only if every inner matcher matches. An empty
/// vector trivially matches everything, so "no matcher kwargs" naturally
/// becomes "match any request" with no special-casing.
struct AllOf(Vec<Box<dyn Match>>);

impl Match for AllOf {
    fn matches(&self, request: &Request) -> bool {
        self.0.iter().all(|matcher| matcher.matches(request))
    }
}

/// Builds an exact-value header matcher. `wiremock::matchers::header` splits
/// the header value on commas (it's designed for multi-value headers like
/// `Accept:`), which silently breaks matching any value that legitimately
/// contains a comma itself — HTTP-date header values being the common case
/// (e.g. `Thu, 01 Jan 1970 00:16:40 GMT`). Route around it with an anchored,
/// escaped `header_regex` instead, which compares the raw value with no
/// splitting.
fn header_exact_matcher(name: HeaderName, value: HeaderValue) -> Box<dyn Match> {
    if let Ok(value) = value.to_str() {
        let pattern = format!("^{}$", regex::escape(value));
        Box::new(matchers::header_regex(name, &pattern))
    } else {
        Box::new(matchers::header(name, value))
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

/// The sole place `MockGuard::received_requests` is read for count checking.
/// wiremock's own `Mock::expect`/`MockServer::verify` are never used (see
/// ARCHITECTURE.md): expectations always stay `Unbounded` on the wiremock
/// side so `Drop for MockGuard` can never panic, and we check counts
/// ourselves via this fully public, non-panicking accessor instead.
///
/// Returns `None` if satisfied (or no `expect:` was given), or a failure
/// message otherwise. Plain `String` rather than `Error` since callers
/// aggregate messages across every entry of a `Mock` object before deciding
/// whether to raise anything.
async fn verify_expect(
    guard: &MockGuard,
    expect: Option<(u64, Option<u64>)>,
    name: Option<&str>,
) -> Option<String> {
    let (min, max) = expect?;
    let n = guard.received_requests().await.len() as u64;
    if n >= min && max.is_none_or(|max| n <= max) {
        None
    } else {
        let label = name.unwrap_or("mock");
        Some(format!(
            "{label}: expected {} matching request(s), received {n}",
            format_expect_range(min, max)
        ))
    }
}

/// Checks every entry's `expect:` and unmounts it (via dropping its guard)
/// regardless of the outcome, aggregating any failures into a single error.
async fn verify_all<'v, 's>(
    strand: &mut Strand<'v, 's>,
    entries: Vec<MockEntry>,
) -> Result<'v, 's, ()> {
    let mut messages = Vec::new();
    for entry in entries {
        let taken = entry.guard.unwrap();
        if let Some(msg) = verify_expect(&taken, entry.expect, entry.name.as_deref()).await {
            messages.push(msg);
        }
        drop(taken);
    }
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
/// variadic argument list) into wiremock matcher/responder/expect/name
/// parts.
///
/// `index` is this spec's position among the specs passed to the same
/// `server.mock` call — it doubles as this entry's index into the `Mock`'s
/// callback-records array (see [`MockObject`]'s slot 1), and identifies it
/// in [`Dispatch`] messages sent by any `CallbackMatch`/`CallbackRespond`
/// this spec registers. `tx` is the channel those callbacks send through;
/// it's only ever cloned into a callback struct if this spec actually uses
/// `match:`/`respond:` callables, so passing it unconditionally costs
/// nothing when a spec is fully declarative.
///
/// Always pushes exactly one element to `callbacks` (nil if this spec used
/// no callback), keeping it 1:1 with the returned entry.
#[cfg_attr(not(feature = "json"), allow(unused_variables, unused_mut))]
async fn parse_mock_spec<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    spec: &Slot<'v, '_>,
    index: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<Dispatch>,
    callbacks: &Array<'v, '_>,
) -> Result<
    'v,
    's,
    (
        Vec<Box<dyn Match>>,
        Responder,
        Option<(u64, Option<u64>)>,
        Option<String>,
        bool,
    ),
> {
    let dict = spec
        .as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, "mock: expected a Dict"))?;

    let (all, response, expect, name, uses_callback) = strand
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
                let mut all: Vec<Box<dyn Match>> = Vec::new();
                let mut response: Option<Responder> = None;
                let mut expect: Option<(u64, Option<u64>)> = None;
                let mut name: Option<String> = None;
                let mut uses_callback = false;

                if dict.get(strand, global.syms.method, None, &mut tmp)? {
                    all.push(Box::new(matchers::method(
                        tmp.to_string(strand)?.to_ascii_uppercase(),
                    )));
                }
                if dict.get(strand, global.syms.path, None, &mut tmp)? {
                    all.push(Box::new(matchers::path(tmp.to_string(strand)?)));
                }
                if dict.get(strand, global.syms.path_regex, None, &mut tmp)? {
                    let pattern = tmp.to_string(strand)?;
                    regex::Regex::new(&pattern)
                        .map_err(|err| Error::value(strand, format!("path_regex: {err}")))?;
                    all.push(Box::new(matchers::path_regex(pattern)));
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
                            all.push(header_exact_matcher(name, hvalue));
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
                            all.push(Box::new(matchers::query_param(
                                key.to_string(strand)?,
                                value.to_string(strand)?,
                            )));
                            Ok(())
                        },
                    )?;
                }
                #[cfg(feature = "json")]
                if dict.get(strand, global.syms.body_json, None, &mut tmp)? {
                    strand.import("json", &mut tmp2).await?;
                    let encode = global.syms.encode;
                    method!(strand, Slot::reborrow(&mut tmp2), encode, &mut tmp3, &tmp).await?;
                    all.push(Box::new(matchers::body_json_string(
                        tmp3.to_string(strand)?,
                    )));
                }
                #[cfg(not(feature = "json"))]
                if dict.get(strand, global.syms.body_json, None, &mut tmp)? {
                    return Err(Error::runtime(
                        strand,
                        "body_json requires the json feature",
                    ));
                }
                if dict.get(strand, global.syms.match_kw, None, &mut match_cb)? {
                    all.push(Box::new(CallbackMatch {
                        index,
                        tx: tx.clone(),
                    }));
                    uses_callback = true;
                }
                if dict.get(strand, global.syms.expect, None, &mut tmp)? {
                    expect = Some(parse_expect(strand, &tmp)?);
                }
                if dict.get(strand, global.syms.name, None, &mut tmp)? {
                    name = Some(tmp.to_string(strand)?);
                }

                if dict.get(strand, global.syms.respond, None, &mut respond_cb)? {
                    if respond_cb.as_dict(strand).is_some() {
                        response = Some(Responder::Static(
                            build_response(strand, global, &respond_cb).await?,
                        ));
                    } else {
                        response = Some(Responder::Callback(CallbackRespond {
                            index,
                            tx: tx.clone(),
                        }));
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

                Ok((all, response, expect, name, uses_callback))
            },
        )
        .await?;

    let response = response.ok_or_else(|| Error::missing_key(strand, "respond"))?;
    Ok((all, response, expect, name, uses_callback))
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
) -> Result<'v, 's, ResponseTemplate> {
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

    let mut template = ResponseTemplate::new(
        StatusCode::from_u16(status.unwrap_or(200))
            .map_err(|_| Error::value(strand, "respond.status out of range"))?,
    );
    for (name, value) in headers {
        template = template.insert_header(name, value);
    }
    if let Some(body) = body {
        template = template.set_body_bytes(body);
    }
    Ok(template)
}

/// Spawns a dedicated OS thread running its own single-threaded tokio
/// runtime, and returns a handle to it plus the means to shut it down.
///
/// This exists solely so that `MockServer::start` (and hence wiremock's
/// internal hyper accept loop, which `tokio::spawn`s onto whatever runtime
/// is ambient when `start` is called) runs on a thread other than the one
/// driving the Do VM. That's required once any mock uses a callback
/// matcher/responder (stage 2): wiremock's `Match`/`Respond` are
/// synchronous and must block their calling thread on a channel round trip
/// to a Do closure running on the main VM thread — blocking the VM's own
/// thread that way would deadlock. Every other wiremock call (registering
/// mocks, resetting, reading received requests) only touches shared
/// `Arc<RwLock<..>>` state with no callback involved, so those stay ordinary
/// `.await`s on the main thread, same as stage 1.
fn spawn_runtime() -> (
    tokio::runtime::Handle,
    tokio::sync::oneshot::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("dolang-http-mock".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build http.mock tokio runtime");
            let _ = handle_tx.send(rt.handle().clone());
            rt.block_on(async move {
                let _ = shutdown_rx.await;
            });
        })
        .expect("failed to spawn http.mock runtime thread");
    let handle = handle_rx
        .recv()
        .expect("http.mock runtime thread failed to start");
    (handle, shutdown_tx, thread)
}

/// Slot 0 holds a Do array of every persistent (non-scoped) `Mock` this
/// server has registered — keeping them reachable independent of whether the
/// script retained the handle `.mock()` returned, so an unreferenced handle
/// can't be GC'd and unmount the mock almost immediately. Scoped mocks are
/// never added here; their lifetime is controlled explicitly by the
/// enclosing `do` block. `Mock::unmount` removes the entry once done.
pub(crate) struct Server {
    inner: Option<MockServer>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort: signal the dedicated thread to stop. Can't join here
        // (Drop isn't async), so this doesn't guarantee the thread has
        // actually exited by the time `Server` is gone — only `.close()`
        // does that. Acceptable since this path only fires for a `Server`
        // that was never explicitly closed (e.g. dropped via GC).
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub(crate) struct ServerAnnex<'v> {
    global: State<'v, Global<'v>>,
}

struct MockEntry {
    guard: Option<MockGuard>,
    expect: Option<(u64, Option<u64>)>,
    name: Option<String>,
}

/// Slot 0 (when present, i.e. this `Mock` came from a persistent, no-block
/// `.mock()` call) holds the owning `Server`, so `.unmount()` can remove
/// itself from that server's keep-alive array (see `Server::SLOTS`) without
/// any Rc/RefCell bookkeeping — liveness is just "reachable in the object
/// graph", the same way the GC already tracks everything else.
///
/// Slot 1 (when present, i.e. at least one of this `Mock`'s entries used a
/// `match:`/`respond:` callback) holds the `strand.Strand` handle for the
/// background strand spawned to run those callbacks (see
/// [`run_callback_strand`]); `.unmount()` cancels it so the strand doesn't
/// linger after every entry's guard has been dropped.
pub(crate) struct MockObject {
    entries: Vec<MockEntry>,
}

pub(crate) type MockObjectAnnex<'v> = State<'v, Global<'v>>;

impl<'v> Object<'v> for Server {
    const NAME: &'v str = "Server";
    const MODULE: &'v str = "http.mock";
    const SLOTS: usize = 1;
    type Annex = ServerAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let ([], [func]) = unpack!(strand, args, 0, 1)?;
        let (runtime, shutdown, thread) = spawn_runtime();
        let server = runtime.spawn(MockServer::start()).await.into_do(strand)?;

        if let Some(func) = func {
            strand
                .with_slots(async move |strand, [mut handle, tmp]| {
                    let value = Server {
                        inner: Some(server),
                        shutdown: Some(shutdown),
                        thread: Some(thread),
                    };
                    this.create_with_annex(strand, value, ServerAnnex { global }, &mut handle);
                    this.cast(&handle)
                        .unwrap()
                        .enter_sync(strand, |strand, instance| {
                            let mut borrow = instance.borrow_mut(strand)?;
                            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), Empty::Array);
                            Ok(())
                        })?;
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
            let value = Server {
                inner: Some(server),
                shutdown: Some(shutdown),
                thread: Some(thread),
            };
            this.create_with_annex(
                strand,
                value,
                ServerAnnex { global },
                Slot::reborrow(&mut out),
            );
            this.cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, instance| {
                    let mut borrow = instance.borrow_mut(strand)?;
                    Output::set(strand, Mut::slot_mut::<0>(&mut borrow), Empty::Array);
                    Ok(())
                })?;
            Ok(())
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let (shutdown, thread) = {
                    let mut borrow = this.borrow_mut(strand)?;
                    drop(borrow.inner.take());
                    (borrow.shutdown.take(), borrow.thread.take())
                };
                if let Some(tx) = shutdown {
                    let _ = tx.send(());
                }
                if let Some(thread) = thread {
                    let _ = tokio::task::spawn_blocking(move || thread.join())
                        .await
                        .into_do(strand)?;
                }
                Ok(())
            })
            .method("reset", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let server = borrow
                    .inner
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "server closed"))?;
                server.reset().await;
                Ok(())
            })
            .method("received_requests", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = this.annex().global;
                let requests = {
                    let borrow = this.borrow(strand)?;
                    let server = borrow
                        .inner
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "server closed"))?;
                    server.received_requests().await
                }
                .unwrap_or_default();
                requests_to_array(strand, global, &requests, out)
            })
            .get("address", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let server = borrow
                    .inner
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "server closed"))?;
                Output::set(strand, out, server.address().to_string().as_str());
                Ok(())
            })
            .get("url", |this, strand, out| {
                let uri = {
                    let borrow = this.borrow(strand)?;
                    let server = borrow
                        .inner
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "server closed"))?;
                    server.uri()
                };
                let url = url::Url::parse(&uri).map_err(|err| Error::runtime(strand, err))?;
                create_url(strand, url, out);
                Ok(())
            })
            .method("mock", async move |this, strand, args, out| {
                let global = this.annex().global;
                strand
                    .with_slots(
                        async move |strand, [mut item, mut callbacks, mut cb_handle]| {
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

                            Output::set(strand, &mut callbacks, Empty::Array);
                            let callbacks_array = callbacks.as_array(strand).unwrap();
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Dispatch>();

                            let mut entries = Vec::new();
                            let mut any_callback = false;
                            for spec in &specs {
                                let (matchers, responder, expect, name, uses_callback) =
                                    parse_mock_spec(
                                        strand,
                                        global,
                                        spec,
                                        entries.len(),
                                        &tx,
                                        &callbacks_array,
                                    )
                                    .await?;
                                any_callback |= uses_callback;
                                let mut wm_mock =
                                    Mock::given(AllOf(matchers)).respond_with(responder);
                                if let Some(name) = &name {
                                    wm_mock = wm_mock.named(name.clone());
                                }
                                let guard = {
                                    let borrow = this.borrow(strand)?;
                                    let server = borrow.inner.as_ref().ok_or_else(|| {
                                        Error::state_error(strand, "server closed")
                                    })?;
                                    server.register_as_scoped(wm_mock).await
                                };
                                entries.push(MockEntry {
                                    guard: Some(guard),
                                    expect,
                                    name,
                                });
                            }

                            // Only spawn a background strand if some entry
                            // actually registered a callback; otherwise this
                            // `Mock` is fully declarative and there is nothing
                            // for it to dispatch.
                            if any_callback {
                                strand.spawn_background(
                                    &callbacks,
                                    None,
                                    &mut cb_handle,
                                    async move |strand, callbacks, _out| {
                                        run_callback_strand(strand, callbacks, rx).await
                                    },
                                )?;
                            }

                            if let Some(block) = block {
                                // Scoped: this `Mock` is never added to the
                                // server's keep-alive array, so nothing else
                                // retains the guards — they're explicitly taken
                                // back out and dropped below regardless of the
                                // block's outcome.
                                let mock_object = MockObject { entries };
                                global.types.mock.create_with_annex(
                                    strand,
                                    mock_object,
                                    global,
                                    &mut item,
                                );
                                if any_callback {
                                    global.types.mock.cast(&item).unwrap().enter_sync(
                                        strand,
                                        |strand, instance| {
                                            let mut mock_borrow = instance.borrow_mut(strand)?;
                                            Output::set(
                                                strand,
                                                Mut::slot_mut::<1>(&mut mock_borrow),
                                                &cb_handle,
                                            );
                                            Ok(())
                                        },
                                    )?;
                                }

                                let result = call!(strand, block, out, &item).await;

                                let verify_result = strand
                                    .with_interrupt_mask(InterruptMask::all(), async |strand| {
                                        let entries =
                                            global.types.mock.cast(&item).unwrap().enter_sync(
                                                strand,
                                                |strand, instance| {
                                                    Ok(std::mem::take(
                                                        &mut instance.borrow_mut(strand)?.entries,
                                                    ))
                                                },
                                            )?;
                                        verify_all(strand, entries).await
                                    })
                                    .await;

                                let cancel_result = strand
                                    .with_interrupt_mask(InterruptMask::all(), async |strand| {
                                        global
                                            .types
                                            .mock
                                            .cast(&item)
                                            .unwrap()
                                            .enter(strand, async |strand, instance| {
                                                cancel_callback_strand(strand, instance, global)
                                                    .await
                                            })
                                            .await
                                    })
                                    .await;

                                let result = combine_results(strand, result, verify_result);
                                combine_results(strand, result, cancel_result)
                            } else {
                                // Persistent: register this `Mock` in the
                                // server's own keep-alive array (so it stays
                                // reachable independent of whether the script
                                // retains the returned handle), and give the
                                // `Mock` a back-reference to the server so
                                // `.unmount()` can remove itself from that array.
                                let mock_object = MockObject { entries };
                                global.types.mock.create_with_annex(
                                    strand,
                                    mock_object,
                                    global,
                                    &mut item,
                                );
                                global.types.mock.cast(&item).unwrap().enter_sync(
                                    strand,
                                    |strand, instance| {
                                        let mut mock_borrow = instance.borrow_mut(strand)?;
                                        Output::set(
                                            strand,
                                            Mut::slot_mut::<0>(&mut mock_borrow),
                                            this,
                                        );
                                        if any_callback {
                                            Output::set(
                                                strand,
                                                Mut::slot_mut::<1>(&mut mock_borrow),
                                                &cb_handle,
                                            );
                                        }
                                        Ok(())
                                    },
                                )?;
                                {
                                    let server_borrow = this.borrow(strand)?;
                                    let array = Ref::slot::<0>(&server_borrow)
                                        .as_array(strand)
                                        .ok_or_else(|| {
                                            Error::state_error(strand, "server closed")
                                        })?;
                                    array.push(strand, &item)?;
                                }
                                Output::set(strand, out, &item);
                                Ok(())
                            }
                        },
                    )
                    .await
            })
    }
}

/// Cancels this `Mock`'s background callback strand (slot 1), if it has
/// one. Shared by the explicit `.unmount()` method and by the scoped
/// `server.mock ... do |mock| ...` cleanup path, so both tear down the
/// strand the same way instead of leaving it running until GC drops the
/// last channel sender.
async fn cancel_callback_strand<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, MockObject>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut cb_handle, mut tmp]| {
            {
                let borrow = this.borrow(strand)?;
                Output::set(strand, &mut cb_handle, Ref::slot::<1>(&borrow));
            }
            if !cb_handle.is_nil() {
                method!(
                    strand,
                    Slot::reborrow(&mut cb_handle),
                    global.syms.cancel,
                    &mut tmp
                )
                .await?;
            }
            Ok(())
        })
        .await
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
            if tmp.eq(strand, target) {
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
    const SLOTS: usize = 2;
    type Annex = MockObjectAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("received", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = *this.annex();
                let mut requests = Vec::new();
                for entry in &this.borrow(strand)?.entries {
                    if let Some(guard) = entry.guard.as_ref() {
                        requests.extend(guard.received_requests().await);
                    }
                }
                requests_to_array(strand, global, &requests, out)
            })
            .method("verify", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut messages = Vec::new();
                for entry in &this.borrow(strand)?.entries {
                    match (entry.guard.as_ref(), entry.expect) {
                        (_, None) => {}
                        (Some(guard), Some(_)) => {
                            if let Some(msg) =
                                verify_expect(guard, entry.expect, entry.name.as_deref()).await
                            {
                                messages.push(msg);
                            }
                        }
                        (None, Some(_)) => {
                            let label = entry.name.as_deref().unwrap_or("mock");
                            messages.push(format!("{label}: unmounted"));
                        }
                    }
                }
                if messages.is_empty() {
                    Ok(())
                } else {
                    Err(Error::runtime(strand, messages.join("\n")))
                }
            })
            .method("unmount", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = *this.annex();
                for entry in &mut this.borrow_mut(strand)?.entries {
                    drop(entry.guard.take());
                }
                cancel_callback_strand(strand, this, global).await?;
                strand.with_slots_sync(|strand, [mut server]| {
                    {
                        let borrow = this.borrow(strand)?;
                        Output::set(strand, &mut server, Ref::slot::<0>(&borrow));
                    }
                    if !server.is_nil()
                        && let Some(server) = global.types.server.cast(&server)
                    {
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
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("http.mock")
        .value("Mock", global.types.mock)
        .value("Request", global.types.request)
        .value("Server", global.types.server)
        .commit();
}
