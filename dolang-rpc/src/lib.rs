//! Framed, multiplexed RPC sessions over asynchronous byte streams.

mod client;
mod handle;
mod opaque;
mod serde;
mod server;
mod transport;

use ::serde::{Serialize, de::DeserializeOwned};
use bytes::{Buf, Bytes, BytesMut};
pub use client::{Call, Client};
#[cfg(any(unix, windows))]
pub use handle::DefaultHandle;
pub use handle::OsHandle;
pub use opaque::{InvalidOpaque, Opaque, OpaqueGuard, OpaqueResource};
pub use server::{CallContext, RequestCancelled, Server};
use transport::{RecvFrame, SendFrame};

const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[repr(C, packed)]
struct Header {
    kind: [u8; 1],
    id: [u8; 8],
    payload_len: [u8; 4],
}

impl Header {
    const LEN: usize = size_of::<Self>();

    fn new(kind: Kind, id: u64, payload_len: u32) -> Self {
        Self {
            kind: [kind as u8],
            id: id.to_le_bytes(),
            payload_len: payload_len.to_le_bytes(),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: Header is packed, contains no padding, and consists only of byte arrays.
        unsafe { std::slice::from_raw_parts(std::ptr::from_ref(self).cast(), Self::LEN) }
    }

    fn decode(bytes: &[u8; Self::LEN]) -> Result<(Kind, u64, usize), Error> {
        let header = unsafe { &*bytes.as_ptr().cast::<Self>() };
        Ok((
            Kind::try_from(header.kind[0])?,
            u64::from_le_bytes(header.id),
            u32::from_le_bytes(header.payload_len) as usize,
        ))
    }
}

/// A family of request and response messages.
pub trait Protocol: Send + Sync + 'static {
    type Request: Serialize + DeserializeOwned + Send + 'static;
    type Response: Serialize + DeserializeOwned + Send + 'static;
}

/// An RPC session error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("deserialization error: {0}")]
    Deserialize(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection closed")]
    ConnectionClosed,
    #[error("request cancelled")]
    Cancelled,
    #[error("transport does not support direct handles")]
    UnsupportedCapability,
}

impl Error {
    fn copy(&self) -> Self {
        match self {
            Self::Io(e) => Self::Io(std::io::Error::new(e.kind(), e.to_string())),
            Self::Serialize(e) => Self::Serialize(e.clone()),
            Self::Deserialize(e) => Self::Deserialize(e.clone()),
            Self::Protocol(e) => Self::Protocol(e.clone()),
            Self::ConnectionClosed => Self::ConnectionClosed,
            Self::Cancelled => Self::Cancelled,
            Self::UnsupportedCapability => Self::UnsupportedCapability,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Kind {
    Request = 1,
    Response = 2,
    Error = 3,
    Cancel = 4,
    Notify = 5,
}

impl TryFrom<u8> for Kind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            3 => Ok(Self::Error),
            4 => Ok(Self::Cancel),
            5 => Ok(Self::Notify),
            _ => Err(Error::Protocol(format!("unknown frame kind {value}"))),
        }
    }
}

async fn read_message<T: RecvFrame>(
    transport: &mut T,
    buffered: &mut BytesMut,
    max: usize,
) -> Result<(Kind, u64, Bytes), Error> {
    loop {
        if buffered.len() >= Header::LEN {
            let header: &[u8; Header::LEN] = buffered[..Header::LEN].try_into().unwrap();
            let (kind, id, len) = Header::decode(header)?;
            if len > max {
                return Err(Error::Protocol(format!(
                    "frame length {len} exceeds limit {max}"
                )));
            }
            let total = Header::LEN + len;
            if buffered.len() >= total {
                let mut message = buffered.split_to(total).freeze();
                message.advance(Header::LEN);
                return Ok((kind, id, message));
            }
            buffered.reserve(total - buffered.len());
        } else {
            buffered.reserve(8192 - buffered.len());
        }
        if transport.recv(buffered).await? == 0 {
            return Err(Error::ConnectionClosed);
        }
    }
}

fn encode<'frame, T: Serialize, F: SendFrame<'frame>>(
    kind: Kind,
    id: u64,
    value: &'frame T,
    frame: &mut F,
) -> Result<Bytes, Error> {
    let buffer = vec![0; Header::LEN];
    let mut buffer =
        serde::to_extend(value, frame, buffer).map_err(|e| Error::Serialize(e.to_string()))?;
    let payload_len = u32::try_from(buffer.len() - Header::LEN)
        .map_err(|_| Error::Protocol("frame is too large".into()))?;
    buffer[..Header::LEN].copy_from_slice(Header::new(kind, id, payload_len).as_bytes());
    Ok(buffer.into())
}

fn encode_empty(kind: Kind, id: u64) -> Bytes {
    Bytes::copy_from_slice(Header::new(kind, id, 0).as_bytes())
}

fn decode<T: DeserializeOwned>(bytes: &[u8], frame: &mut impl RecvFrame) -> Result<T, Error> {
    serde::from_bytes(bytes, frame).map_err(|e| Error::Deserialize(e.to_string()))
}
