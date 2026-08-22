#![deny(warnings)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//! Framed, multiplexed RPC sessions over asynchronous byte streams.
//!
//! Define a [`Protocol`], negotiate a transport with [`Builder`], then bind
//! the negotiated endpoint to that protocol. The client may issue concurrent
//! calls; [`Server::serve`](server::Server::serve) dispatches concurrent request handlers.
//!
//! ```no_run
//! use dolang_rpc::{Builder, Protocol, server::CallContext};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize, Serialize)]
//! enum Request { Ping }
//! #[derive(Deserialize, Serialize)]
//! enum Response { Pong }
//! struct Example;
//! impl Protocol for Example {
//!     type Request = Request;
//!     type Response = Response;
//! }
//!
//! async fn run() -> Result<(), Box<dyn std::error::Error>> {
//!     let (client_io, server_io) = tokio::io::duplex(16 * 1024);
//!     let (client, server) = tokio::try_join!(
//!         Builder::new("example", &[1]).client(client_io),
//!         Builder::new("example", &[1]).server(server_io),
//!     )?;
//!
//!     let server = async {
//!         server.bind::<Example>().serve(async |mut context: CallContext<Example>, request| {
//!             context.shutdown();
//!             match request {
//!                 Request::Ping => context.respond(Response::Pong),
//!             }
//!         }).await
//!     };
//!     let client = async {
//!         let response = client.bind::<Example>().call(Request::Ping).await?.into_response();
//!         assert!(matches!(response, Response::Pong));
//!         Ok::<_, dolang_rpc::Error>(())
//!     };
//!     let (server, client) = tokio::join!(server, client);
//!     server?;
//!     client?;
//!     Ok(())
//! }
//! ```

pub mod auth;
pub mod client;
mod driver;
#[cfg(target_os = "macos")]
mod escrow;
mod fragment;
pub mod handle;
mod serde;
pub mod server;
pub mod session;
pub mod trailer;
mod transport;
mod unbound;
mod window;

use std::io;

use ::serde::{Serialize, de::DeserializeOwned};
pub use unbound::Builder;

/// Configurable size and concurrency limits for a session. Not public — set
/// via [`Builder`]'s chainable setters instead.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Maximum size of one whole wire fragment, header included. Bounds how
    /// much of a large message is written per round-robin turn; the header
    /// is subtracted from this to get the actual payload budget per write.
    pub max_fragment_size: usize,
    /// Maximum size of one complete (reassembled) message's postcard
    /// payload, excluding any trailer.
    pub max_payload_size: usize,
    /// Maximum number of native handles attached to one wire fragment.
    pub max_handles_per_fragment: usize,
    /// Maximum number of native handles carried by one message.
    pub max_handles_per_message: usize,
    /// Maximum trailer fragment payload copied immediately by the receive
    /// driver when the consumer has not yet requested that fragment. Set to
    /// zero to disable copying nonempty fragments on this path.
    pub trailer_recv_copy_threshold: usize,
    /// Maximum trailer fragment payload copied immediately by the receive
    /// driver when the consumer is already waiting for that fragment. Set to
    /// zero to disable copying nonempty fragments on this path.
    pub trailer_recv_demand_copy_threshold: usize,
    /// Maximum trailer fragment payload copied into staging by
    /// `TrailerSend::poll_write` without first waiting for a transport grant.
    /// Set to zero to disable copying nonempty fragments on this path.
    pub trailer_send_copy_threshold: usize,
    /// Maximum number of concurrent calls, counted from the first fragment
    /// of a request to its response.
    ///
    /// A call spends that span in two custodians — the reassembler while its
    /// payload is still arriving, then the endpoint until it answers — and
    /// the limit is on the sum, so a peer cannot get twice the budget by
    /// keeping half its calls in each. Messages that have finished their
    /// payload and entered their trailer phase are *not* counted: those are
    /// bounded by the credit window below rather than by a count, because a
    /// trailer may legitimately outlive its call.
    ///
    /// This is a count, not a memory bound. What bounds the memory those
    /// calls hold is `max_outstanding_payload` below, which is why this can
    /// be generous: a peer may keep a great many *small* calls in flight
    /// without thereby being allowed to keep that many large ones.
    pub max_concurrent_calls: usize,
    /// Total charged postcard bytes across all calls that have not yet
    /// released, in aggregate.
    ///
    /// `max_payload_size` bounds one message and cannot bound the sum:
    /// multiplied by `max_concurrent_calls` it is the whole reassembly
    /// footprint a peer can demand, reachable by opening that many messages
    /// and sending one fragment of each. This bounds the sum directly, and
    /// spans the *whole call lifecycle* rather than just reassembly — a
    /// payload's memory does not end at dispatch, it ends when the
    /// application is done with the call. That makes it a byte-denominated
    /// concurrency bound, which is the point.
    ///
    /// Unlike a trailer, a postcard payload cannot be paced incrementally: it
    /// has to be reassembled whole before it can be deserialized. So the two
    /// rules invert — trailers have no size cap because they are streamable,
    /// and payloads keep one because they are not.
    ///
    /// The pool measures **wire** bytes. The deserialized form is
    /// `O(serialized size)`, so wire bytes are an adequate proxy, but a
    /// struct-heavy payload can land at 4–8× its postcard size once padding
    /// and per-node overhead are counted. Size the limit knowing that.
    ///
    /// Trailer bytes are not counted here; see `trailer_session_window`.
    /// Negotiation keeps this at least `max_payload_size`, since otherwise a
    /// legal single message could never be sent.
    pub max_outstanding_payload: usize,
    /// How much retired trailer credit this end accumulates before returning
    /// it to the peer.
    ///
    /// Purely a local coalescing knob — it is not negotiated and bounds
    /// nothing. Larger values mean fewer `Kind::Credit` fragments and a
    /// coarser feedback signal; a few fragments' worth is the useful range.
    /// Credit is always flushed regardless once the trailer ends, the pool is
    /// exhausted, or a consumer is left waiting for bytes, so no value can
    /// stall a sender — including a sender that budgets the pool across its
    /// own trailers, which this end never learns about.
    pub trailer_credit_interval: usize,
    /// Bytes of unretired trailer data all trailers on the session may have
    /// outstanding, in aggregate.
    ///
    /// "Unretired" means the consuming application has not yet released the
    /// credit for it — deliberately later than having read it, so this bounds
    /// staged bytes plus whatever the application still holds. A sender parks
    /// once the pool is empty and resumes on the next `Kind::Credit`.
    ///
    /// This is the only credit limit trailers have, and the whole bound on
    /// receiver memory attributable to them, however many are open. Payload
    /// quota is a separate pool (`max_outstanding_payload`); sharing one
    /// between the two deadlocks a handler that must consume a trailer before
    /// it can release its payload. There is deliberately no per-trailer
    /// window: a sender that lets one trailer consume the pool
    /// starves only its own other trailers, so how the pool is divided is a
    /// local scheduling choice rather than a protocol rule. Only zero is a
    /// deadlock, and negotiation floors it at 1.
    pub trailer_session_window: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_fragment_size: 512 * 1024,
            max_payload_size: 2 * 1024 * 1024,
            max_handles_per_fragment: 8,
            max_handles_per_message: 8,
            trailer_recv_copy_threshold: 64 * 1024,
            trailer_recv_demand_copy_threshold: 256 * 1024,
            trailer_send_copy_threshold: 64 * 1024,
            max_concurrent_calls: 1024,
            max_outstanding_payload: 16 * 1024 * 1024,
            trailer_credit_interval: 256 * 1024,
            trailer_session_window: 16 * 1024 * 1024,
        }
    }
}

/// Maximum size of a `fragment::Kind::Negotiate` fragment, tolerated by both ends of a
/// connection regardless of their configured `Limits`. Negotiation must use a
/// fixed, transport-independent bound rather than `Limits::max_fragment_size`
/// because neither side knows what the peer will actually enforce until
/// negotiation completes. Not configurable — a future refactor must not tie
/// this to `Limits`.
pub(crate) const NEGOTIATE_FRAGMENT_SIZE: usize = 1024;

/// Maximum total size of a reassembled `fragment::Kind::Negotiate` message payload,
/// across all of its fragments. Bounds how much a peer can make the
/// receiving end buffer before negotiation (and with it, the negotiated
/// `Limits`) is in force. A real handshake payload — version blobs plus an
/// application-protocol name and version list — is at most a few hundred
/// bytes; this leaves generous headroom without allowing unbounded growth.
/// Not configurable, for the same reason as `NEGOTIATE_FRAGMENT_SIZE`.
pub(crate) const NEGOTIATE_MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// A family of messages exchanged by one RPC session.
///
/// Implement this marker trait once for each application protocol version
/// represented by distinct Rust request and response types. Both peers must
/// bind the negotiated connection to compatible implementations.
pub trait Protocol: Send + Sync + 'static {
    /// Messages sent by [`client::Client`] calls and received by
    /// [`server::Server`] handlers.
    type Request: Serialize + DeserializeOwned + Send + 'static;
    /// Messages returned by [`server::Server`] handlers and yielded by
    /// completed [`client::Call`]s.
    type Response: Serialize + DeserializeOwned + Send + 'static;
}

/// An error from session establishment, transport, or an individual call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying transport failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// Serializing an outgoing request or response failed.
    #[error("serialization error: {0}")]
    Serialize(String),
    /// Deserializing an incoming request or response failed.
    #[error("deserialization error: {0}")]
    Deserialize(String),
    /// The peer sent data that violates the RPC protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A pre-shared key was rejected, missing, or unexpected.
    ///
    /// Covers both a locally supplied key that cannot be used (see
    /// [`AuthKey::new`](crate::auth::AuthKey::new)) and a peer that failed the check during negotiation.
    /// The message never includes key material.
    #[error("authentication error: {0}")]
    Auth(String),
    /// The local or peer session closed before the operation completed.
    #[error("connection closed")]
    ConnectionClosed,
    /// The peer cancelled this call before it received a response.
    #[error("request cancelled")]
    Cancelled,
    /// A requested transport capability is unavailable.
    ///
    /// Returned when an [`OsHandle`](crate::handle::OsHandle) is serialized
    /// over a session whose transport cannot carry handle attachments (a
    /// generic byte-stream transport rather than a Unix-domain socket or a
    /// Windows named pipe).
    #[error("transport does not support direct handles")]
    UnsupportedCapability,
}

impl Error {
    pub(crate) fn copy(&self) -> Self {
        match self {
            Self::Io(e) => Self::Io(io::Error::new(e.kind(), e.to_string())),
            Self::Serialize(e) => Self::Serialize(e.clone()),
            Self::Deserialize(e) => Self::Deserialize(e.clone()),
            Self::Protocol(e) => Self::Protocol(e.clone()),
            Self::Auth(e) => Self::Auth(e.clone()),
            Self::ConnectionClosed => Self::ConnectionClosed,
            Self::Cancelled => Self::Cancelled,
            Self::UnsupportedCapability => Self::UnsupportedCapability,
        }
    }
}
