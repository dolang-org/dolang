//! Staged construction: negotiate first, choose a concrete [`Protocol`]
//! afterward.
//!
//! [`Client<P>`](crate::client::Client)/[`Server<P>`](crate::server::Server) are generic over
//! a statically known `P`, but which concrete `P` to use can depend on the
//! *negotiated* application-protocol version (e.g. a future protocol
//! revision might be represented as a distinct Rust type). The client and
//! server [`Unbound`](crate::client::Unbound) endpoints negotiate an
//! application protocol first, expose what was negotiated, and only then let
//! the caller bind to a concrete `P`.
//!
//! [`Builder`] is the sole entry point for constructing either one: it takes
//! the mandatory application-protocol descriptor up front, offers chainable
//! setters for individual size/concurrency limits, and a terminal method per
//! transport shape (`client`/`client_split`/... or `server`/`server_split`/...).

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
use std::{os::unix::net::UnixStream, result};

#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
#[cfg(all(docsrs, not(windows)))]
struct OwnedHandle;

#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};
#[cfg(all(docsrs, not(windows)))]
struct NamedPipeClient;
#[cfg(all(docsrs, not(windows)))]
struct NamedPipeServer;

use crate::{
    Error, Limits, Protocol,
    auth::{Auth, AuthKey},
    client::Client,
    fragment,
    server::Server,
    transport,
};

/// Builds an unbound client or server endpoint.
///
/// A builder advertises one application-protocol name and supported versions.
/// Its terminal `client*` or `server*` method consumes it, performs the
/// handshake, and returns an unbound endpoint that can be inspected before
/// binding it to a concrete [`Protocol`]. Limit setters override defaults;
/// size and concurrency limits are negotiated to the more conservative value,
/// while copy thresholds are local performance settings.
pub struct Builder {
    name: String,
    versions: Vec<u16>,
    limits: Limits,
    key: Option<AuthKey>,
}

impl Builder {
    /// Starts a builder for an application-protocol name and supported versions.
    ///
    /// `versions` must be nonempty and unique.
    pub fn new(name: &str, versions: &[u16]) -> Self {
        Self {
            name: name.to_owned(),
            versions: versions.to_vec(),
            limits: Limits::default(),
            key: None,
        }
    }

    /// Requires mutual proof of a pre-shared key during negotiation.
    ///
    /// Both endpoints must be configured with the same key, or neither: a
    /// mismatch in either direction aborts the handshake. See [`crate::auth`]
    /// for what this does and does not protect against.
    pub fn key(mut self, key: AuthKey) -> Self {
        self.key = Some(key);
        self
    }

    /// Sets the maximum complete wire-fragment size, including its header.
    ///
    /// This bounds one round-robin write of a fragmented message. Defaults to
    /// 512 KiB; the peer and local endpoint use the smaller advertised value.
    pub fn max_fragment_size(mut self, value: usize) -> Self {
        self.limits.max_fragment_size = value;
        self
    }

    /// Sets the maximum reassembled postcard payload, excluding a trailer.
    ///
    /// Defaults to 2 MiB; the peer and local endpoint use the smaller
    /// advertised value, and it is lowered further to the negotiated
    /// [`max_outstanding_payload`](Self::max_outstanding_payload) if that
    /// ends up smaller — a per-message cap above the aggregate pool would
    /// describe a message that could never be sent.
    pub fn max_payload_size(mut self, value: usize) -> Self {
        self.limits.max_payload_size = value;
        self
    }

    /// Sets the session-wide postcard payload quota, in bytes.
    ///
    /// This bounds the total charged payload bytes of every call that has not
    /// yet released, across the whole connection. Unlike
    /// [`max_payload_size`](Self::max_payload_size), which bounds one message,
    /// this bounds the sum — and it is charged for the *entire call
    /// lifecycle*, from the sender admitting the message to the receiving
    /// application being done with it, not merely while the payload is being
    /// reassembled. A payload's memory does not end at dispatch.
    ///
    /// The consequence is a contract worth knowing: a long-pending call with a
    /// large payload holds its share of the pool for as long as it pends, and
    /// throttles the connection accordingly. Indefinitely pending calls are
    /// legitimate — an event poll is the usual shape — and a large payload on
    /// one is unusual but not unthinkable. If you want both, release
    /// explicitly (see
    /// [`CallContext::release_payload`](crate::server::CallContext::release_payload)
    /// and [`CallResult::take_payload_credit`](crate::client::CallResult::take_payload_credit))
    /// once you no longer need the request. Violating it degrades throughput;
    /// it does not hang anything.
    ///
    /// Trailer bytes are counted against
    /// [`trailer_session_window`](Self::trailer_session_window) instead, and
    /// the two pools are deliberately separate: a handler that must consume a
    /// trailer before it can release its payload would deadlock against a
    /// shared one.
    ///
    /// The pool measures wire bytes, and a struct-heavy payload can occupy
    /// several times that once deserialized. Defaults to 16 MiB; the peer and
    /// local endpoint use the smaller advertised value, raised to at least
    /// this endpoint's own `max_payload_size` before it is advertised.
    pub fn max_outstanding_payload(mut self, value: usize) -> Self {
        self.limits.max_outstanding_payload = value;
        self
    }

    /// Sets how much retired trailer credit accumulates before it is
    /// returned to the peer, in bytes.
    ///
    /// Purely a local coalescing knob: it is not negotiated, the two ends
    /// need not agree on it, and it bounds nothing — what bounds the peer is
    /// [`trailer_session_window`](Self::trailer_session_window). Larger
    /// values mean fewer credit fragments and a coarser feedback signal.
    /// Credit is flushed regardless once the pool is exhausted or a trailer
    /// ends, so no value can stall a sender.
    ///
    /// Defaults to 256 KiB.
    pub fn trailer_credit_interval(mut self, value: usize) -> Self {
        self.limits.trailer_credit_interval = value;
        self
    }

    /// Sets the session-wide trailer credit pool, in bytes.
    ///
    /// This bounds trailer data the peer has sent but this end has not yet
    /// retired — released by the consuming application, which is later than
    /// merely reading it — across *all* trailers at once. It is the only
    /// credit limit and the whole bound on receiver memory attributable to
    /// trailers, however many are open. There is no separate cap on a
    /// trailer's total size, so a trailer may stream indefinitely.
    ///
    /// There is deliberately no per-trailer subdivision: a sender that lets
    /// one trailer consume the pool starves only its own other trailers, so
    /// dividing it up is the sending end's local scheduling choice rather
    /// than a protocol rule — and any division it chooses is safe, since
    /// credit is flushed whenever a consumer is left waiting and not merely
    /// at the coalescing threshold. The corollary is that a consumer which
    /// stalls indefinitely can hold as much of the pool as the peer chose to
    /// spend on it.
    ///
    /// Defaults to 16 MiB; the peer and local endpoint use the smaller
    /// advertised value, floored at 1. A value below
    /// [`max_fragment_size`](Self::max_fragment_size) is legal but merely
    /// produces short fragments.
    pub fn trailer_session_window(mut self, value: usize) -> Self {
        self.limits.trailer_session_window = value;
        self
    }

    /// Sets the maximum native handles carried by one wire fragment.
    ///
    /// Defaults to 8, is capped to the transport's operating-system limit,
    /// and is negotiated down to the peer's advertised value.
    pub fn max_handles_per_fragment(mut self, value: usize) -> Self {
        self.limits.max_handles_per_fragment = value;
        self
    }

    /// Sets the maximum native handles carried by one message.
    ///
    /// Defaults to 8; the peer and local endpoint use the smaller
    /// advertised value.
    pub fn max_handles_per_message(mut self, value: usize) -> Self {
        self.limits.max_handles_per_message = value;
        self
    }

    /// Sets the receive-side eager-copy threshold for an undemanded fragment.
    ///
    /// A fragment at or below this size is copied immediately, allowing the
    /// connection receive loop to continue without waiting for the trailer
    /// reader. Defaults to 64 KiB. Set zero to disable nonempty eager copies.
    pub fn trailer_recv_copy_threshold(mut self, value: usize) -> Self {
        self.limits.trailer_recv_copy_threshold = value;
        self
    }

    /// Sets the receive-side eager-copy threshold for a demanded fragment.
    ///
    /// This applies when the trailer reader is already waiting for the next
    /// fragment. Defaults to 256 KiB. Set zero to disable nonempty eager
    /// copies on this path.
    pub fn trailer_recv_demand_copy_threshold(mut self, value: usize) -> Self {
        self.limits.trailer_recv_demand_copy_threshold = value;
        self
    }

    /// Sets the send-side staging threshold for a trailer fragment.
    ///
    /// A write at or below this size is copied into staging without waiting
    /// for a transport grant. Defaults to 64 KiB. Set zero to disable
    /// nonempty eager staging.
    pub fn trailer_send_copy_threshold(mut self, value: usize) -> Self {
        self.limits.trailer_send_copy_threshold = value;
        self
    }

    /// Sets the maximum number of concurrent calls, counted from a request's
    /// first fragment to its response.
    ///
    /// Requests still being reassembled count against it alongside those
    /// already dispatched, so the two together can never exceed this.
    /// Messages that have entered their trailer phase are excluded — a
    /// trailer may outlive its call, and is bounded by
    /// [`trailer_session_window`](Self::trailer_session_window) instead.
    ///
    /// This is a count and not a memory bound; what bounds the memory those
    /// calls hold is
    /// [`max_outstanding_payload`](Self::max_outstanding_payload), which is
    /// why the default is generous.
    ///
    /// Defaults to 1024; the peer and local endpoint use the smaller
    /// advertised value.
    pub fn max_concurrent_calls(mut self, value: usize) -> Self {
        self.limits.max_concurrent_calls = value;
        self
    }

    fn app_protocol(&self) -> (&str, &[u16]) {
        (&self.name, &self.versions)
    }

    fn client_auth(&self) -> Option<Auth> {
        self.key.map(|key| key.as_client())
    }

    fn server_auth(&self) -> Option<Auth> {
        self.key.map(|key| key.as_server())
    }

    /// Negotiates a client session over a bidirectional byte stream.
    pub async fn client<T>(self, stream: T) -> Result<crate::client::Unbound, Error>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, receiver) = transport::generic_duplex(stream);
        negotiate_client(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            self.limits,
            #[cfg(windows)]
            None,
            self.app_protocol(),
            self.client_auth(),
        )
        .await
    }

    /// Negotiates a client session over separate byte-stream reader and writer
    /// halves.
    pub async fn client_split<R, W>(
        self,
        reader: R,
        writer: W,
    ) -> Result<crate::client::Unbound, Error>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let (sender, receiver) = transport::generic(reader, writer);
        negotiate_client(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            self.limits,
            #[cfg(windows)]
            None,
            self.app_protocol(),
            self.client_auth(),
        )
        .await
    }

    #[cfg(unix)]
    /// Negotiates a client session over a connected Unix domain socket.
    ///
    /// Unlike [`client`](Self::client), this transport supports direct
    /// [`OsHandle`](crate::handle::OsHandle) attachments.
    pub async fn client_unix(self, stream: UnixStream) -> Result<crate::client::Unbound, Error> {
        let (sender, receiver) = transport::unix::unix(stream)?;
        negotiate_client(
            transport::AnySender::Unix(sender),
            transport::AnyReceiver::Unix(receiver),
            self.limits,
            #[cfg(windows)]
            None,
            self.app_protocol(),
            self.client_auth(),
        )
        .await
    }

    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    /// Starts a client session on the server end of a Windows named pipe.
    ///
    /// `peer_process` is retained for the lifetime of the session and must
    /// grant process-query and synchronization access. Construction fails if
    /// it does not identify the named-pipe peer.
    ///
    /// # Safety
    ///
    /// The identified peer must be trusted to send only handle values that it
    /// created in this process with `DuplicateHandle`. A malicious peer can
    /// otherwise cause this process to close arbitrary handles.
    pub async unsafe fn client_named_pipe_server(
        self,
        pipe: NamedPipeServer,
        peer_process: OwnedHandle,
    ) -> Result<crate::client::Unbound, Error> {
        #[cfg(windows)]
        {
            crate::client::validate_peer_process(
                &peer_process,
                transport::windows::server_pipe_peer_pid(&pipe)?,
            )?;
            let (sender, receiver) = transport::windows::server_pipe(pipe, false)?;
            negotiate_client(
                transport::AnySender::Windows(sender),
                transport::AnyReceiver::Windows(receiver),
                self.limits,
                Some(peer_process),
                self.app_protocol(),
                self.client_auth(),
            )
            .await
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = (self, pipe, peer_process);
            unreachable!()
        }
    }

    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    /// Starts a client session on the client end of a Windows named pipe.
    ///
    /// `peer_process` is retained for the lifetime of the session and must
    /// grant process-query and synchronization access. Construction fails if
    /// it does not identify the named-pipe peer.
    ///
    /// # Safety
    ///
    /// The identified peer must be trusted to send only handle values that it
    /// created in this process with `DuplicateHandle`. A malicious peer can
    /// otherwise cause this process to close arbitrary handles.
    pub async unsafe fn client_named_pipe_client(
        self,
        pipe: NamedPipeClient,
        peer_process: OwnedHandle,
    ) -> Result<crate::client::Unbound, Error> {
        #[cfg(windows)]
        {
            crate::client::validate_peer_process(
                &peer_process,
                transport::windows::client_pipe_peer_pid(&pipe)?,
            )?;
            let (sender, receiver) = transport::windows::client_pipe(pipe, false)?;
            negotiate_client(
                transport::AnySender::Windows(sender),
                transport::AnyReceiver::Windows(receiver),
                self.limits,
                Some(peer_process),
                self.app_protocol(),
                self.client_auth(),
            )
            .await
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = (self, pipe, peer_process);
            unreachable!()
        }
    }

    /// Negotiates a server session over a bidirectional byte stream.
    pub async fn server<T>(self, stream: T) -> Result<crate::server::Unbound, Error>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, receiver) = transport::generic_duplex(stream);
        negotiate_server(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            self.limits,
            self.app_protocol(),
            self.server_auth(),
        )
        .await
    }

    /// Negotiates a server session over separate byte-stream reader and writer
    /// halves.
    pub async fn server_split<R, W>(
        self,
        reader: R,
        writer: W,
    ) -> Result<crate::server::Unbound, Error>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let (sender, receiver) = transport::generic(reader, writer);
        negotiate_server(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            self.limits,
            self.app_protocol(),
            self.server_auth(),
        )
        .await
    }

    #[cfg(unix)]
    /// Negotiates a server session over a connected Unix domain socket.
    ///
    /// Unlike [`server`](Self::server), this transport supports direct
    /// [`OsHandle`](crate::handle::OsHandle) attachments.
    pub async fn server_unix(
        self,
        stream: UnixStream,
    ) -> result::Result<crate::server::Unbound, Error> {
        let (sender, receiver) = transport::unix::unix(stream)?;
        negotiate_server(
            transport::AnySender::Unix(sender),
            transport::AnyReceiver::Unix(receiver),
            self.limits,
            self.app_protocol(),
            self.server_auth(),
        )
        .await
    }

    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    /// Creates a server on the server end of a Windows named pipe.
    pub async fn server_named_pipe_server(
        self,
        pipe: NamedPipeServer,
    ) -> Result<crate::server::Unbound, Error> {
        #[cfg(windows)]
        {
            let (sender, receiver) = transport::windows::server_pipe(pipe, true)?;
            negotiate_server(
                transport::AnySender::Windows(sender),
                transport::AnyReceiver::Windows(receiver),
                self.limits,
                self.app_protocol(),
                self.server_auth(),
            )
            .await
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = (self, pipe);
            unreachable!()
        }
    }

    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    /// Creates a server on the client end of a Windows named pipe.
    pub async fn server_named_pipe_client(
        self,
        pipe: NamedPipeClient,
    ) -> Result<crate::server::Unbound, Error> {
        #[cfg(windows)]
        {
            let (sender, receiver) = transport::windows::client_pipe(pipe, true)?;
            negotiate_server(
                transport::AnySender::Windows(sender),
                transport::AnyReceiver::Windows(receiver),
                self.limits,
                self.app_protocol(),
                self.server_auth(),
            )
            .await
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = (self, pipe);
            unreachable!()
        }
    }
}

async fn negotiate_client(
    mut sender: transport::AnySender,
    mut receiver: transport::AnyReceiver,
    limits: Limits,
    #[cfg(windows)] peer_process: Option<OwnedHandle>,
    app_protocol: (&str, &[u16]),
    auth: Option<Auth>,
) -> Result<UnboundClient, Error> {
    // The RPC framing version itself is an implementation detail,
    // uninteresting once binding to `P` — only the application-protocol
    // version negotiated below is surfaced.
    let negotiated =
        fragment::negotiate(&mut sender, &mut receiver, &limits, app_protocol, auth).await?;
    receiver.set_max_handles_per_fragment(negotiated.limits.max_handles_per_fragment);
    Ok(UnboundClient {
        sender,
        receiver,
        limits: negotiated.limits,
        #[cfg(windows)]
        peer_process,
        app_protocol: negotiated.app_protocol,
    })
}

async fn negotiate_server(
    mut sender: transport::AnySender,
    mut receiver: transport::AnyReceiver,
    limits: Limits,
    app_protocol: (&str, &[u16]),
    auth: Option<Auth>,
) -> Result<UnboundServer, Error> {
    // The RPC framing version itself is an implementation detail,
    // uninteresting once binding to `P` — only the application-protocol
    // version negotiated below is surfaced.
    let negotiated =
        fragment::negotiate(&mut sender, &mut receiver, &limits, app_protocol, auth).await?;
    receiver.set_max_handles_per_fragment(negotiated.limits.max_handles_per_fragment);
    Ok(UnboundServer {
        sender,
        receiver,
        limits: negotiated.limits,
        app_protocol: negotiated.app_protocol,
    })
}

pub struct UnboundClient {
    sender: transport::AnySender,
    receiver: transport::AnyReceiver,
    limits: Limits,
    #[cfg(windows)]
    peer_process: Option<OwnedHandle>,
    app_protocol: (String, u16),
}

impl UnboundClient {
    /// The negotiated application protocol name.
    pub fn name(&self) -> &str {
        &self.app_protocol.0
    }

    /// The negotiated application protocol version.
    pub fn version(&self) -> u16 {
        self.app_protocol.1
    }

    /// Consumes this endpoint and binds it to a concrete protocol type.
    ///
    /// The caller is responsible for choosing a `P` compatible with the
    /// negotiated application protocol name and version.
    pub fn bind<P: Protocol>(self) -> Client<P> {
        Client::from_transport(
            self.sender,
            self.receiver,
            self.limits,
            #[cfg(windows)]
            self.peer_process,
        )
    }
}

pub struct UnboundServer {
    sender: transport::AnySender,
    receiver: transport::AnyReceiver,
    limits: Limits,
    app_protocol: (String, u16),
}

impl UnboundServer {
    /// The negotiated application protocol name.
    pub fn name(&self) -> &str {
        &self.app_protocol.0
    }

    /// The negotiated application protocol version.
    pub fn version(&self) -> u16 {
        self.app_protocol.1
    }

    /// Consumes this endpoint and binds it to a concrete protocol type.
    ///
    /// The caller is responsible for choosing a `P` compatible with the
    /// negotiated application-protocol name and version.
    pub fn bind<P: Protocol>(self) -> Server<P> {
        Server::from_transport(self.sender, self.receiver, self.limits)
    }
}
