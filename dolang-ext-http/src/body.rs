//! Request bodies read from byte handles.
//!
//! A body that isn't a `Str` or `Bin` is read from its own stream if it provides one, or
//! else from its items through `io::concat`. Native targets stream each handle while the
//! request is sent. The browser's fetch API cannot stream request bodies, so on wasm each
//! handle is read to the end before sending.

use dolang::runtime::{
    Error, Result, Strand, Value,
    error::ErrorKind,
    io::{self, IoCaps},
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) use browser::Bodies;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) use native::Bodies;

/// Returns a reader over `value`'s bytes.
///
/// A value that provides a byte stream is read from it, and must be readable. Any other
/// value is read as its [items].
pub(crate) async fn reader<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Box<dyn io::Read>> {
    let handle = io::get(strand, value, |strand, caps| {
        if caps.contains(IoCaps::READ) {
            Ok(())
        } else {
            Err(Error::value(strand, "body must be readable"))
        }
    });
    match handle {
        Ok(handle) => read(strand, handle),
        Err(err) if err.kind() == ErrorKind::Unsupported => items(strand, value, b"").await,
        Err(err) => Err(err),
    }
}

/// Returns a reader over the bytes of `value`'s items, each followed by `end`.
///
/// A value that isn't iterable raises a `TypeError`.
pub(crate) async fn items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    end: &[u8],
) -> Result<'v, 's, Box<dyn io::Read>> {
    let handle = strand
        .with_slots(async |strand, [mut iterator]| {
            value.iter(strand, &mut iterator).await?;
            io::concat(strand, &iterator, end)
        })
        .await?;
    read(strand, handle)
}

fn read<'v, 's>(
    strand: &mut Strand<'v, 's>,
    handle: io::ByteHandle,
) -> Result<'v, 's, Box<dyn io::Read>> {
    handle
        .into_read()
        .map_err(|_| Error::value(strand, "body must be readable"))
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
mod native {
    use std::{
        fmt,
        sync::{Arc, Mutex},
    };

    use bytes::Bytes;
    use dolang::runtime::{Result, Strand, io};
    use futures::{AsyncReadExt as _, stream};

    use crate::http::ResultExt as _;

    /// The most bytes one read of a body takes.
    const CHUNK_SIZE: usize = 64 * 1024;

    /// The error reqwest sees when a body fails; the real one waits in [`Bodies`].
    #[derive(Debug)]
    struct BodyError;

    impl fmt::Display for BodyError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "request body stream error")
        }
    }

    impl std::error::Error for BodyError {}

    /// Streamed request bodies, and the first error any of them raised.
    pub(crate) struct Bodies {
        error: Arc<Mutex<Option<std::io::Error>>>,
    }

    impl Bodies {
        pub(crate) fn new() -> Self {
            Self {
                error: Arc::new(Mutex::new(None)),
            }
        }

        /// Returns a body that streams `reader` while the request is sent.
        pub(crate) async fn add<'v, 's>(
            &mut self,
            _strand: &mut Strand<'v, 's>,
            reader: Box<dyn io::Read>,
        ) -> Result<'v, 's, reqwest::Body> {
            let error = self.error.clone();
            let stream = stream::try_unfold(reader, move |mut reader| {
                let error = error.clone();
                async move {
                    let mut buf = vec![0; CHUNK_SIZE];
                    match reader.read(&mut buf).await {
                        Ok(0) => Ok(None),
                        Ok(len) => {
                            buf.truncate(len);
                            Ok(Some((Bytes::from(buf), reader)))
                        }
                        Err(err) => {
                            if let Ok(mut slot) = error.lock() {
                                slot.get_or_insert(err);
                            }
                            Err(BodyError)
                        }
                    }
                }
            });
            Ok(reqwest::Body::wrap_stream(stream))
        }

        /// Sends the request, failing with a body's own error if one failed.
        pub(crate) async fn send<'v, 's>(
            self,
            strand: &mut Strand<'v, 's>,
            builder: reqwest::RequestBuilder,
        ) -> Result<'v, 's, reqwest::Response> {
            let response = builder.send().await;
            if let Some(err) = self.error.lock().ok().and_then(|mut slot| slot.take()) {
                return Err(io::error(strand, err));
            }
            response.into_http(strand)
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use dolang::runtime::{
        Result, Strand,
        io::{self, IoResultExt as _},
    };
    use futures::AsyncReadExt as _;

    use crate::http::ResultExt as _;

    /// Request bodies read into memory, since fetch can't stream them.
    pub(crate) struct Bodies;

    impl Bodies {
        pub(crate) fn new() -> Self {
            Self
        }

        /// Returns a body holding the rest of `reader`.
        pub(crate) async fn add<'v, 's>(
            &mut self,
            strand: &mut Strand<'v, 's>,
            mut reader: Box<dyn io::Read>,
        ) -> Result<'v, 's, reqwest::Body> {
            let mut body = Vec::new();
            reader.read_to_end(&mut body).await.into_do_io(strand)?;
            Ok(reqwest::Body::from(body))
        }

        /// Sends the request.
        pub(crate) async fn send<'v, 's>(
            self,
            strand: &mut Strand<'v, 's>,
            builder: reqwest::RequestBuilder,
        ) -> Result<'v, 's, reqwest::Response> {
            builder.send().await.into_http(strand)
        }
    }
}
