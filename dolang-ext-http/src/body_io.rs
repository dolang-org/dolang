//! Response bodies as Do `io.Io` streams.
//!
//! On native targets the body becomes a [`ByteHandle`](dolang::runtime::io::ByteHandle),
//! so native consumers read it directly. The browser's body stream holds JavaScript
//! values, which can't leave the VM thread, so on wasm the body is a native `Io` instead,
//! and `Io`'s bridge serves its byte handle on the VM thread.

use std::pin::Pin;

use bytes::Bytes;
use dolang::runtime::{Slot, State, Strand};
use futures::stream::Stream;

use crate::global::Global;

/// Body of a response as a stream of chunks.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Body of a response as a stream of chunks.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>>>>;

/// Creates a Do `Io` that reads `body`.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) fn create<'v>(
    strand: &mut Strand<'v, '_>,
    _global: State<'v, Global<'v>>,
    body: ByteStream,
    out: Slot<'v, '_>,
) {
    use dolang::runtime::{Output, io::ByteHandle};

    Output::set(strand, out, ByteHandle::read(native::BodyRead::new(body)));
}

/// Creates a Do `Io` that reads `body`.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) fn create<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    body: ByteStream,
    out: Slot<'v, '_>,
) {
    global
        .types
        .body_io
        .create(strand, browser::BodyIo::new(body), out);
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
mod native {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll, ready},
    };

    use bytes::{Buf as _, Bytes};
    use futures::io::AsyncRead;

    use super::ByteStream;

    /// Reads a body's chunks as bytes.
    pub(super) struct BodyRead {
        stream: ByteStream,
        /// The unread rest of the current chunk.
        chunk: Bytes,
    }

    impl BodyRead {
        pub(super) fn new(stream: ByteStream) -> Self {
            Self {
                stream,
                chunk: Bytes::new(),
            }
        }
    }

    impl AsyncRead for BodyRead {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            while self.chunk.is_empty() {
                match ready!(self.stream.as_mut().poll_next(cx)) {
                    Some(Ok(chunk)) => self.chunk = chunk,
                    Some(Err(err)) => return Poll::Ready(Err(io::Error::other(err))),
                    None => return Poll::Ready(Ok(0)),
                }
            }
            let len = buf.len().min(self.chunk.len());
            buf[..len].copy_from_slice(&self.chunk[..len]);
            self.chunk.advance(len);
            Poll::Ready(Ok(len))
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) use browser::BodyIo;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use bytes::{Buf as _, Bytes};
    use dolang::runtime::{Error, Object, Output, object::TypeBuilder, unpack, value::TypeObject};
    use futures::stream::StreamExt as _;

    use super::ByteStream;
    use crate::http::ResultExt as _;

    /// A response body read on the VM thread.
    pub(crate) struct BodyIo {
        /// The body, until the stream is closed.
        stream: Option<ByteStream>,
        /// The unread rest of the current chunk.
        chunk: Bytes,
    }

    impl BodyIo {
        pub(super) fn new(stream: ByteStream) -> Self {
            Self {
                stream: Some(stream),
                chunk: Bytes::new(),
            }
        }
    }

    impl<'v> Object<'v> for BodyIo {
        const NAME: &'v str = "ResponseBody";
        const MODULE: &'v str = "http";
        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

        fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
            builder
                .supertype(TypeObject::Io)
                .get("readable", |this, strand, out| {
                    let readable = this.borrow(strand)?.stream.is_some();
                    Output::set(strand, out, readable);
                    Ok(())
                })
                .method("read", async move |this, strand, args, out| {
                    let ([size], []) = unpack!(strand, args, 1, 0)?;
                    let size = size
                        .to_i64(strand)
                        .ok()
                        .and_then(|n| usize::try_from(n).ok());
                    let size = size.ok_or_else(|| {
                        Error::type_error(strand, "size must be a non-negative integer")
                    })?;
                    let mut borrow = this.borrow_mut(strand)?;
                    let BodyIo { stream, chunk } = &mut *borrow;
                    let stream = stream
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "closed"))?;
                    while size > 0 && chunk.is_empty() {
                        match stream.next().await.transpose().into_http(strand)? {
                            Some(next) => *chunk = next,
                            None => break,
                        }
                    }
                    let len = size.min(chunk.len());
                    Output::set(strand, out, &chunk[..len]);
                    chunk.advance(len);
                    Ok(())
                })
                .method("close", async move |this, strand, args, _out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.stream = None;
                    borrow.chunk = Bytes::new();
                    Ok(())
                })
        }
    }
}
