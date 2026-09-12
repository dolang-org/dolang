//! Request bodies built from Do iterators.
//!
//! Native targets stream each iterator through a channel that a pump strand
//! fills while the request is sent. The browser's fetch API cannot stream
//! request bodies, so on wasm each iterator is collected before sending.

use dolang::runtime::{Result, Strand, Value, value::fmt::Format};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) use browser::IterBodies;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) use native::IterBodies;

struct BytesFormat<'a>(&'a mut Vec<u8>);

impl<'v> Format<'v> for BytesFormat<'_> {
    fn write_str<'s>(&mut self, _strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()> {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Appends the bytes of one body item, followed by a newline for `lines:`.
fn push_item<'v, 's>(
    strand: &mut Strand<'v, 's>,
    item: &Value<'v>,
    lines: bool,
    buf: &mut Vec<u8>,
) -> Result<'v, 's, ()> {
    if let Some(slice) = item.as_bin(strand) {
        strand.access(|x| buf.extend_from_slice(slice.as_slice(x)));
    } else {
        item.display(strand, &mut BytesFormat(buf))?;
    }

    if lines {
        buf.push(b'\n');
    }
    Ok(())
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
mod native {
    use std::{
        fmt,
        pin::Pin,
        result,
        task::{Context, Poll},
    };

    use bytes::Bytes;
    use dolang::runtime::{Error, Output, Result, Slot, Strand, Value, value::Empty};
    use futures::stream::Stream;
    use tokio::sync::mpsc;

    use super::push_item;
    use crate::http::ResultExt as _;

    /// Custom error type for body streaming errors
    #[derive(Debug)]
    struct BodyError;

    impl fmt::Display for BodyError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "request body stream error")
        }
    }

    impl std::error::Error for BodyError {}

    type Sender = mpsc::Sender<result::Result<Bytes, BodyError>>;

    /// Stream wrapper for tokio mpsc receiver
    struct BodyStream(mpsc::Receiver<result::Result<Bytes, BodyError>>);

    impl Stream for BodyStream {
        type Item = result::Result<Bytes, BodyError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    struct Pump {
        /// Index of the iterator in the roots array.
        index: usize,
        sender: Sender,
        lines: bool,
    }

    /// Streamed request bodies awaiting their pumps.
    pub(crate) struct IterBodies<'v, 'b, 'r> {
        /// Holds an array rooting each pumped iterator once a body is added.
        roots: &'r mut Slot<'v, 'b>,
        pumps: Vec<Pump>,
    }

    impl<'v, 'b, 'r> IterBodies<'v, 'b, 'r> {
        pub(crate) fn new(roots: &'r mut Slot<'v, 'b>) -> Self {
            Self {
                roots,
                pumps: Vec::new(),
            }
        }

        /// Returns a body that streams `iterator`'s items while the request is
        /// sent.
        pub(crate) async fn add<'s>(
            &mut self,
            strand: &mut Strand<'v, 's>,
            iterator: &Value<'v>,
            lines: bool,
        ) -> Result<'v, 's, reqwest::Body> {
            if self.pumps.is_empty() {
                Output::set(strand, &mut *self.roots, Empty::Array);
            }
            let roots = self
                .roots
                .as_array(strand)
                .ok_or_else(|| Error::state_error(strand, "request body roots missing"))?;
            let index = roots.len(strand)?;
            roots.push(strand, iterator)?;
            let (sender, receiver) = mpsc::channel(8);
            self.pumps.push(Pump {
                index,
                sender,
                lines,
            });
            Ok(reqwest::Body::wrap_stream(BodyStream(receiver)))
        }

        /// Sends the request, running the pumps for any streamed bodies
        /// concurrently.
        pub(crate) async fn send<'s>(
            self,
            strand: &mut Strand<'v, 's>,
            builder: reqwest::RequestBuilder,
        ) -> Result<'v, 's, reqwest::Response> {
            if self.pumps.is_empty() {
                return builder.send().await.into_http(strand);
            }

            let roots = &*self.roots;
            let pumps = self
                .pumps
                .into_iter()
                .map(|pump| {
                    strand.spawn_scoped(None, async move |strand| {
                        strand
                            .with_slots(async move |strand, [mut iterator]| {
                                let roots = roots.as_array(strand).ok_or_else(|| {
                                    Error::state_error(strand, "request body roots missing")
                                })?;
                                if !roots.get(strand, pump.index, &mut iterator)? {
                                    return Err(Error::state_error(
                                        strand,
                                        "request body root missing",
                                    ));
                                }
                                pump_request_body(strand, &iterator, pump.sender, pump.lines).await
                            })
                            .await
                    })
                })
                .collect::<Vec<_>>();

            let (response, pumps) =
                futures::join!(builder.send(), futures::future::join_all(pumps));

            for pump in pumps {
                pump?;
            }

            response.into_http(strand)
        }
    }

    /// Pumps data from a VM iterator into a channel for request body streaming
    async fn pump_request_body<'v, 's>(
        strand: &mut Strand<'v, 's>,
        iterator: &Value<'v>,
        sender: Sender,
        lines: bool,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(async move |strand, [mut item]| {
                loop {
                    match iterator.next(strand, &mut item).await {
                        Ok(true) => {
                            let mut vec = Vec::new();
                            push_item(strand, &item, lines, &mut vec)?;

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
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use std::marker::PhantomData;

    use dolang::runtime::{Result, Slot, Strand, Value};

    use super::push_item;
    use crate::http::ResultExt as _;

    /// Request bodies collected in memory, since fetch can't stream them.
    pub(crate) struct IterBodies<'v, 'b, 'r>(PhantomData<&'r mut Slot<'v, 'b>>);

    impl<'v, 'b, 'r> IterBodies<'v, 'b, 'r> {
        pub(crate) fn new(_roots: &'r mut Slot<'v, 'b>) -> Self {
            Self(PhantomData)
        }

        /// Returns a body holding all of `iterator`'s items.
        pub(crate) async fn add<'s>(
            &mut self,
            strand: &mut Strand<'v, 's>,
            iterator: &Value<'v>,
            lines: bool,
        ) -> Result<'v, 's, reqwest::Body> {
            strand
                .with_slots(async move |strand, [mut item]| {
                    let mut body = Vec::new();
                    while iterator.next(strand, &mut item).await? {
                        push_item(strand, &item, lines, &mut body)?;
                    }
                    Ok(reqwest::Body::from(body))
                })
                .await
        }

        /// Sends the request.
        pub(crate) async fn send<'s>(
            self,
            strand: &mut Strand<'v, 's>,
            builder: reqwest::RequestBuilder,
        ) -> Result<'v, 's, reqwest::Response> {
            builder.send().await.into_http(strand)
        }
    }
}
