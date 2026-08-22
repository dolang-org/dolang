//! The handling side of a bound RPC session.
//!
//! [`Server::serve`] dispatches each incoming request to a handler
//! concurrently with the others, passing it a [`CallContext`] used to send
//! the response.

#[cfg(windows)]
use std::{any::TypeId, io, os::windows::io::OwnedHandle};
use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use futures::{
    StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    Error, Limits, Protocol,
    driver::{Drain, DrainSignal, DrainWatch, drain_signal},
    fragment::{self, Event, Kind, Message},
    serde::{decode_payload, encode_payload},
    session::{self, Cite, Gift, InvalidOpaque, OpaqueGuard, OpaqueResource, Session},
    trailer::{RecvShared, SendShared, TrailerRecv, TrailerSend},
    transport::{self, EncodeHandles, Receiver, Sender},
    window::{ControlSink, PayloadBudget, PayloadCharge, SessionWindow},
};
#[cfg(windows)]
use crate::{handle::TakeHandle, session::Inner as OpaqueInner};

#[cfg(windows)]
struct DecodeHandles<'a> {
    receiver: &'a transport::AnyReceiver,
    session: &'a Arc<Session>,
    count: usize,
    max_handles: usize,
}

#[cfg(windows)]
impl TakeHandle for DecodeHandles<'_> {
    fn take_handle(&mut self, value: usize) -> io::Result<OwnedHandle> {
        if self.count == self.max_handles {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message contains too many handle attachments",
            ));
        }
        self.count += 1;
        self.receiver.duplicate_peer_handle(value)
    }

    fn take_gift(&mut self, owner: u8, id: u64) -> io::Result<OpaqueInner> {
        self.session
            .take_gift(owner, id)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid opaque reference"))
    }

    fn take_cite(&mut self, owner: u8, id: u64, marker: TypeId) -> io::Result<OpaqueInner> {
        self.session
            .take_cite(owner, id, marker)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid opaque reference"))
    }
}

/// A negotiated server endpoint that has not yet been bound to a [`Protocol`].
///
/// Inspect its negotiated application protocol, then consume it with
/// [`bind`](Unbound::bind) to obtain a [`Server`].
pub use crate::unbound::UnboundServer as Unbound;

/// A server endpoint for one connection.
///
/// Consume it with [`serve`](Self::serve) to dispatch requests from the peer.
pub struct Server<P: Protocol> {
    sender: transport::AnySender,
    receiver: transport::AnyReceiver,
    outgoing: mpsc::UnboundedSender<Outgoing<P::Response>>,
    outgoing_rx: mpsc::UnboundedReceiver<Outgoing<P::Response>>,
    /// The other end of `Inner::shutdown`, held until `serve` hands it to
    /// the receive driver.
    shutdown_rx: oneshot::Receiver<()>,
    /// Held until `serve` hands the two ends to the two drivers.
    drain: (DrainSignal, DrainWatch),
    shared: Arc<Shared>,
    marker: PhantomData<fn() -> P>,
}

enum Outgoing<R> {
    Response {
        id: u64,
        value: R,
        trailer: fragment::Trailer,
    },
    Error {
        id: u64,
    },
    Cancel {
        id: u64,
    },
    /// We stopped reading a request trailer (it arrived unwanted) and want
    /// to tell the peer to stop sending it. Always results in a wire
    /// `Kind::Discard` fragment.
    DiscardTrailer {
        id: u64,
    },
    /// A wire `Kind::Discard` fragment arrived, telling us the peer no
    /// longer wants our response trailer. Applied to our own active send;
    /// never re-sent to the peer.
    PeerDiscarded {
        id: u64,
    },
    Ack {
        id: u64,
    },
    /// We retired `count` bytes of the request trailer on `id` and are
    /// returning that much credit. Always results in a wire `Kind::Credit`.
    Credit {
        id: u64,
        count: u32,
    },
    /// A call released its request payload and is returning `count` bytes of
    /// quota. Always results in a wire `Kind::PayloadCredit`, which names no
    /// message: several of these coalesce into one fragment.
    PayloadCredit {
        count: u32,
    },
    /// Drops `count` of this endpoint's references to the peer's opaque `id`.
    Release {
        id: u64,
        count: u32,
    },
}

/// Emits `Release` frames for opaques whose last local handle dropped. The
/// strong senders stay exactly what they were: `serve`'s own sender and each
/// live `CallContext`.
impl<R: Send + 'static> session::ReleaseSink for mpsc::WeakUnboundedSender<Outgoing<R>> {
    fn release(&self, id: u64, count: u32) {
        // Called from `Drop`, so a departed channel is not an error: the
        // writer is already gone and the peer's table dies with the session.
        if let Some(outgoing) = self.upgrade() {
            let _ = outgoing.send(Outgoing::Release { id, count });
        }
    }
}

impl<R: Send + 'static> ControlSink for mpsc::WeakUnboundedSender<Outgoing<R>> {
    fn credit(&self, id: u64, count: u32) {
        if let Some(outgoing) = self.upgrade() {
            let _ = outgoing.send(Outgoing::Credit { id, count });
        }
    }

    fn payload_credit(&self, count: u32) {
        // Reached from `PayloadCharge::drop`, which runs on every path a call
        // can end on — including ones where the session is already tearing
        // down, where there is no one left to credit.
        if let Some(outgoing) = self.upgrade() {
            let _ = outgoing.send(Outgoing::PayloadCredit { count });
        }
    }

    fn discard(&self, id: u64) {
        // Reached from `TrailerRecv::drop`, so a departed channel just means
        // the connection is already gone and the peer has nothing to stop.
        if let Some(outgoing) = self.upgrade() {
            let _ = outgoing.send(Outgoing::DiscardTrailer { id });
        }
    }
}

/// State both connection drivers and every live handler share.
///
/// One `Arc` rather than a bundle of them, and held strongly by all three:
/// nothing in here can keep the session alive on its own, because the ability
/// to still get a message out is the `outgoing` sender, which stays outside
/// it. Closing that channel is still what shuts the writer down.
struct Shared {
    inner: Mutex<Inner>,
    session: Arc<Session>,
    /// Send-side trailer credit shared by every outgoing response trailer on
    /// this connection. Bounds what the peer must buffer for us in aggregate.
    trailer_session: Arc<SessionWindow>,
    /// Send-side payload quota shared by every outgoing response. Bounds the
    /// postcard bytes the peer must hold for us across all live calls, and is
    /// charged in full when a response is admitted to the scheduler.
    ///
    /// Kept apart from `trailer_session` on purpose; see [`crate::window`].
    payload_budget: Arc<PayloadBudget>,
    limits: Limits,
}

impl Shared {
    /// Maximum handle attachments one message may carry.
    fn max_handles(&self) -> usize {
        // A transport configured to attach no handles to a fragment can
        // carry none at all.
        #[cfg(unix)]
        if self.limits.max_handles_per_fragment == 0 {
            return 0;
        }
        self.limits.max_handles_per_message
    }

    /// Finishes handle encoding for message `id`, taking custody of whatever
    /// this platform must keep alive once the message is on the wire.
    ///
    /// On macOS that is the file descriptors themselves, escrowed until the
    /// peer acknowledges receipt. Every other unix passes them with the
    /// fragment and is done with them.
    #[cfg(unix)]
    fn finish_handles(&self, id: u64, handles: EncodeHandles) -> transport::OutgoingHandles {
        let handles = handles.finish();
        #[cfg(target_os = "macos")]
        if handles.needs_ack() {
            self.inner.lock().unwrap().fd_escrow.register(id);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = id;
        handles
    }

    /// Finishes handle encoding for message `_id`. Windows duplicates each
    /// handle into the peer as it is encoded, so the originals are this
    /// end's to close and nothing is escrowed.
    #[cfg(windows)]
    fn finish_handles(&self, _id: u64, handles: EncodeHandles) -> transport::OutgoingHandles {
        let (handles, escrow) = handles.finish();
        drop(escrow);
        handles
    }

    /// Decodes a message payload, taking custody of every handle and opaque
    /// reference it carries.
    #[cfg(unix)]
    fn decode<T: ::serde::de::DeserializeOwned>(
        &self,
        payload: &[u8],
        handles: transport::ReceivedHandles,
        _receiver: &transport::AnyReceiver,
    ) -> Result<T, Error> {
        decode_payload(
            payload,
            &mut session::SessionHandles {
                inner: handles,
                session: &self.session,
            },
        )
    }

    /// Decodes a message payload, taking custody of every handle and opaque
    /// reference it carries. Windows handles are named by value in the
    /// payload and duplicated out of the peer as they are decoded, rather
    /// than arriving attached to the fragment, so `handles` is empty.
    #[cfg(windows)]
    fn decode<T: ::serde::de::DeserializeOwned>(
        &self,
        payload: &[u8],
        _handles: transport::ReceivedHandles,
        receiver: &transport::AnyReceiver,
    ) -> Result<T, Error> {
        decode_payload(
            payload,
            &mut DecodeHandles {
                receiver,
                session: &self.session,
                count: 0,
                max_handles: self.max_handles(),
            },
        )
    }

    /// Records the file descriptors for `id` that just reached the wire.
    #[cfg(target_os = "macos")]
    fn escrow_sent(&self, id: u64, fds: Vec<std::os::fd::OwnedFd>, done: bool) {
        self.inner.lock().unwrap().fd_escrow.sent(id, fds, done);
    }

    /// Forgets the escrow for a message that will never reach the wire.
    fn discard_unsent_escrow(&self, id: u64) {
        #[cfg(target_os = "macos")]
        self.inner.lock().unwrap().fd_escrow.discard_unsent(id);
        #[cfg(not(target_os = "macos"))]
        let _ = id;
    }

    /// Releases the escrow an `Ack` names, returning false when there is
    /// none — which is every `Ack` on a platform that escrows nothing.
    fn release_escrow(&self, id: u64) -> bool {
        #[cfg(target_os = "macos")]
        return self.inner.lock().unwrap().fd_escrow.release(id);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = id;
            false
        }
    }
}

struct Inner {
    outstanding: HashMap<u64, Cancellation>,
    /// Signals the receive driver to stop accepting new work. Taken by the
    /// first handler to ask for shutdown, so later ones are no-ops.
    shutdown: Option<oneshot::Sender<()>>,
    #[cfg(target_os = "macos")]
    fd_escrow: crate::escrow::FdEscrow,
}

struct Cancellation {
    signal: Option<oneshot::Sender<()>>,
    abort: AbortHandle,
}

/// Refuses a fragment the peer had no business sending, before the
/// reassembler can allocate anything for it.
///
/// Unlike the client's gate, this needs no id: a client never asks this end
/// for anything, so a response in this direction names nothing at all.
fn check_header(header: &fragment::FragmentHeader) -> Result<(), Error> {
    if header.kind == Kind::Response {
        return Err(Error::Protocol(
            "server received a Response fragment".into(),
        ));
    }
    Ok(())
}

impl<P: Protocol> Server<P> {
    /// Builds a `Server` from an already-negotiated transport. Only reachable
    /// via [`Unbound::bind`] — `Server` has
    /// no public constructors of its own, so it's never possible to hold one
    /// that hasn't already completed `fragment::negotiate`, and `serve`
    /// never needs to negotiate itself.
    pub(crate) fn from_transport(
        sender: transport::AnySender,
        receiver: transport::AnyReceiver,
        limits: Limits,
    ) -> Self {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        Self {
            sender,
            receiver,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    outstanding: HashMap::new(),
                    shutdown: Some(shutdown),
                    #[cfg(target_os = "macos")]
                    fd_escrow: Default::default(),
                }),
                session: Session::new(Box::new(outgoing.downgrade())),
                trailer_session: Arc::new(SessionWindow::new(limits.trailer_session_window)),
                payload_budget: Arc::new(PayloadBudget::new(limits.max_outstanding_payload)),
                limits,
            }),
            outgoing,
            outgoing_rx,
            shutdown_rx,
            drain: drain_signal(),
            marker: PhantomData,
        }
    }

    /// Serves requests until the peer disconnects, the session fails, or a
    /// handler requests graceful shutdown.
    ///
    /// The handler may be called concurrently for independent requests. Each
    /// invocation must consume its [`CallContext`] with [`CallContext::respond`]
    /// or [`CallContext::respond_with_trailer`]; dropping the context without
    /// responding reports a per-request error to the peer.
    pub async fn serve<H>(self, handler: H) -> Result<(), Error>
    where
        H: AsyncFn(CallContext<P>, P::Request) + Send + Sync + 'static,
    {
        let (drain_signal, drain_watch) = self.drain;
        let send = SendDriver::<P>::new(
            self.sender,
            self.outgoing_rx,
            self.shared.clone(),
            drain_watch,
        )
        .run();
        tokio::pin!(send);
        let recv = RecvDriver::new(
            self.receiver,
            self.outgoing,
            self.shutdown_rx,
            self.shared,
            handler,
            drain_signal,
        )
        .run();
        tokio::pin!(recv);
        let result = tokio::select! {
            result = &mut recv => result,
            // A successful send-side exit is not the end of the connection.
            // In practice graceful mode keeps the driver alive until the
            // receiver ends, so this only covers the ordinary channel-close
            // path. A send failure is fatal immediately because no further
            // receive-side progress can repair it.
            result = &mut send => {
                result?;
                return match recv.await {
                    Err(Error::ConnectionClosed) => Ok(()),
                    result => result,
                };
            }
        };
        // The receive half ended first, so the session is failing or the peer
        // is gone. It published `Drain::Abrupt` on the way out; the send
        // driver flushes what it had already committed to the wire and
        // abandons anything still waiting on credit that can no longer
        // arrive. Dropping `recv` above also dropped its sender, so nothing
        // new can be queued behind it either.
        result.and(send.await)
    }
}

/// Drives the receive half of the connection: reassembles inbound fragments,
/// dispatches requests to the handler, and owns the tear-down that follows
/// whatever ends the session.
///
/// This is the half that runs on the caller's own future rather than a task
/// of its own, because it is what [`Server::serve`] returns the result of.
struct RecvDriver<P: Protocol, H> {
    transport: transport::AnyReceiver,
    reassembler: fragment::Reassembler,
    shared: Arc<Shared>,
    /// Kept alive for the whole run: dropping this and every handler task's
    /// clone is what closes the send driver's channel and shuts it down.
    outgoing: mpsc::UnboundedSender<Outgoing<P::Response>>,
    handler: Arc<H>,
    /// Fires when a handler has asked to shut down gracefully.
    shutdown: oneshot::Receiver<()>,
    /// Tells the send driver how much it still owes; see [`crate::driver`].
    drain: DrainSignal,
}

impl<P: Protocol, H> RecvDriver<P, H>
where
    H: AsyncFn(CallContext<P>, P::Request) + Send + Sync + 'static,
{
    fn new(
        transport: transport::AnyReceiver,
        outgoing: mpsc::UnboundedSender<Outgoing<P::Response>>,
        shutdown: oneshot::Receiver<()>,
        shared: Arc<Shared>,
        handler: H,
        drain: DrainSignal,
    ) -> Self {
        let reassembler = fragment::Reassembler::new(shared.limits, Arc::new(outgoing.downgrade()));
        Self {
            transport,
            reassembler,
            shared,
            outgoing,
            handler: Arc::new(handler),
            shutdown,
            drain,
        }
    }

    /// Applies `max_concurrent_calls` to a call that is arriving or starting
    /// to arrive.
    ///
    /// A concurrent call is one this end has begun receiving and has not yet
    /// answered, and it passes through two custodians on the way: the
    /// reassembler holds it while its payload is still fragmented, and
    /// `outstanding` holds it from dispatch until the response head. The
    /// limit is on the *sum* — the two counts are disjoint, since a message
    /// leaves payload phase in the same `accept` call that dispatches it — so
    /// neither custodian can enforce it alone, and checking them separately
    /// would admit twice the limit.
    ///
    /// `incomplete` is the reassembler's count *including* the call being
    /// admitted, so callers add one for a call that has already left payload
    /// phase.
    fn check_call_admission(&self, id: u64, incomplete: usize) -> Result<(), Error> {
        let inner = self.shared.inner.lock().unwrap();
        let duplicate = inner.outstanding.contains_key(&id);
        let outstanding = inner.outstanding.len();
        if duplicate {
            return Err(Error::Protocol(format!("duplicate active request id {id}")));
        }
        if outstanding + incomplete > self.shared.limits.max_concurrent_calls {
            return Err(Error::Protocol("too many concurrent calls".into()));
        }
        Ok(())
    }

    /// Runs until the peer disconnects or the session fails.
    ///
    /// A handler asking to shut down does *not* end this — it starts a
    /// drain. New requests are refused from that point, but the transport
    /// keeps being read, because the calls already dispatched still have to
    /// answer and the flow-control credit their responses may be waiting on
    /// arrives through this half. Once those handlers have finished, this
    /// driver publishes [`Drain::Graceful`] and keeps reading until the peer
    /// closes its transport, including after the send driver has emptied its
    /// scheduler.
    ///
    /// Beyond publishing that signal it knows nothing of the send driver.
    async fn run(mut self) -> Result<(), Error> {
        let mut tasks = FuturesUnordered::new();
        // Set when a handler has asked to shut down. From then on the
        // `shutdown` branch is disarmed (a consumed `oneshot` resolves
        // immediately and would spin the loop) and new requests are refused.
        let mut draining = false;
        let result = 'main: loop {
            let mut frame = self.transport.recv();
            // The header/payload reads must not be dropped and restarted
            // once they've begun: any bytes already consumed from the
            // transport into their local buffers would otherwise be lost,
            // desynchronizing the stream. `step` is polled repeatedly by
            // the inner loop below (never recreated) so that racing it
            // against `tasks.next()` and `continue`-ing loses no progress.
            let complete = {
                let step = async {
                    let header = fragment::read_fragment_header(&mut frame).await?;
                    check_header(&header)?;
                    self.reassembler.accept(header, &mut frame).await
                };
                tokio::pin!(step);
                loop {
                    tokio::select! {
                        result = &mut step => break result,
                        // Handler tasks must keep being polled here: a
                        // handler reading a request trailer is unblocked by
                        // the very fragment this read is fetching. Their
                        // completions are also the trigger for sealing the
                        // drain, since a drain ends when the last dispatched
                        // call has answered.
                        Some(_) = tasks.next(), if !tasks.is_empty() => {
                            self.drain.seal_if_idle(draining, tasks.is_empty());
                            continue;
                        }
                        _ = &mut self.shutdown, if !draining => {
                            draining = true;
                            self.drain.seal_if_idle(draining, tasks.is_empty());
                            continue;
                        }
                    }
                }
            };
            let complete = match complete {
                Ok(complete) => complete,
                Err(error) => break 'main Err(error),
            };
            let (message, live_trailer) = match complete {
                Event::None => (None, None),
                // A request has started arriving. It occupies the same
                // budget as one already dispatched, so it is admitted on the
                // same rule, at the earliest point this end knows about it.
                Event::PayloadIncomplete { id } => {
                    if let Err(error) =
                        self.check_call_admission(id, self.reassembler.payload_incomplete())
                    {
                        break 'main Err(error);
                    }
                    (None, None)
                }
                Event::Aborted {
                    kind: Kind::Request,
                    ..
                } => (None, None),
                Event::Aborted { kind, .. } => {
                    break 'main Err(Error::Protocol(format!(
                        "unexpected aborted {kind:?} message"
                    )));
                }
                Event::Message(message) => (Some(message), None),
                Event::Ack { id, message } => {
                    let _ = self.outgoing.send(Outgoing::Ack { id });
                    (message, None)
                }
                Event::Trailer {
                    shared: trailer,
                    len,
                    ..
                } => (None, Some((trailer, len))),
                Event::Release { id, count } => {
                    self.shared.session.release(id, count);
                    (None, None)
                }
                Event::Credit { id, count } => {
                    // Applied here rather than routed through the writer;
                    // see the client's matching arm.
                    self.shared.trailer_session.refund(id, count as usize);
                    (None, None)
                }
                Event::PayloadCredit { count } => {
                    self.shared.payload_budget.credit(count as usize);
                    (None, None)
                }
            };
            if let Some(Message {
                kind,
                id,
                payload,
                handles,
                trailer,
                charge,
            }) = message
            {
                match kind {
                    Kind::Request if draining => {
                        // A drain finishes the calls already dispatched; it
                        // does not take on new ones. Refusing here rather
                        // than letting the reassembler reject it keeps the
                        // decision in one place, and dropping `charge`
                        // returns the request's payload quota to the peer —
                        // this end is still reading, so that credit is still
                        // worth sending.
                        //
                        // The trailer is wrapped before being dropped rather
                        // than dropped as it arrived: `TrailerRecv`'s `Drop`
                        // is what tells the peer to stop sending, and a
                        // refused request that left its trailer streaming
                        // would go on consuming the drain it is not part of.
                        let _ = self.outgoing.send(Outgoing::Error { id });
                        drop(trailer.map(TrailerRecv::new));
                        drop(charge);
                    }
                    Kind::Request => {
                        // This message has already left payload phase, so it
                        // is no longer in the reassembler's count and has to
                        // be added back.
                        if let Err(error) =
                            self.check_call_admission(id, self.reassembler.payload_incomplete() + 1)
                        {
                            break Err(error);
                        }
                        let request = match self.shared.decode(&payload, handles, &self.transport) {
                            Ok(request) => request,
                            Err(error) => break Err(error),
                        };
                        let trailer = trailer.map(TrailerRecv::new);
                        let handler = self.handler.clone();
                        let task_shared = self.shared.clone();
                        let task_outgoing = self.outgoing.clone();
                        let (abort, registration) = AbortHandle::new_pair();
                        tasks.push(Abortable::new(
                            async move {
                                let context = CallContext {
                                    id,
                                    shared: task_shared,
                                    request_trailer: trailer,
                                    outgoing: task_outgoing,
                                    responded: false,
                                    shutdown_on_respond: false,
                                    charge: Some(charge),
                                    marker: PhantomData,
                                };
                                handler(context, request).await;
                            },
                            registration,
                        ));
                        self.shared.inner.lock().unwrap().outstanding.insert(
                            id,
                            Cancellation {
                                signal: None,
                                abort,
                            },
                        );
                    }
                    Kind::Cancel => {
                        let mut state = self.shared.inner.lock().unwrap();
                        if let Some(signal) = state
                            .outstanding
                            .get_mut(&id)
                            .and_then(|cancel| cancel.signal.take())
                        {
                            let _ = signal.send(());
                        } else if let Some(cancel) = state.outstanding.get(&id) {
                            cancel.abort.abort();
                        } else {
                            let _ = self.outgoing.send(Outgoing::Cancel { id });
                        }
                    }
                    Kind::Discard => {
                        let _ = self.outgoing.send(Outgoing::PeerDiscarded { id });
                    }
                    Kind::Ack => {
                        if !self.shared.release_escrow(id) {
                            break Err(Error::Protocol(format!(
                                "Ack for response {id} with no active escrow"
                            )));
                        }
                    }
                    _ => {
                        break Err(Error::Protocol(format!("unexpected {kind:?} frame")));
                    }
                }
            }
            if let Some((trailer, len)) = live_trailer {
                let frame = self.transport.recv();
                // SAFETY: the lease retains the receiver borrow and clears
                // the erased token before it ends.
                let lease = unsafe { RecvShared::grant(&trailer, frame, len) };
                let result = loop {
                    tokio::select! {
                        result = RecvShared::wait_fragment(&trailer) => break result,
                        Some(_) = tasks.next(), if !tasks.is_empty() => {
                            self.drain.seal_if_idle(draining, tasks.is_empty());
                            continue;
                        }
                        _ = &mut self.shutdown, if !draining => {
                            draining = true;
                            self.drain.seal_if_idle(draining, tasks.is_empty());
                            continue;
                        }
                    }
                };
                if let Err(error) = result {
                    break 'main Err(error.into());
                }
                lease.complete();
            }
        };
        drop(self.transport);
        if draining {
            // A drain was already under way when the transport failed, so the
            // calls already dispatched still get to finish — their responses
            // queue up behind the send driver's channel even though most will
            // no longer reach the peer.
            while tasks.next().await.is_some() {}
        }
        // Reaching here at all means this half is over, so the send driver
        // must not be left waiting on credit that can now never arrive. This
        // is deliberately unconditional and deliberately last: it overrides
        // any `Graceful` already published, including one this very drain
        // set a moment ago before the transport gave out.
        self.drain.set(Drain::Abrupt);
        if draining && matches!(&result, Err(Error::ConnectionClosed)) {
            Ok(())
        } else {
            result
        }
    }
}

/// Drives the send half of the connection: admits queued messages into the
/// fragment scheduler and advances the scheduler onto the transport.
///
/// Runs on [`Server::serve`]'s own future, alongside the receive driver and
/// the handlers — a response is queued rather than written by the handler
/// that produced it, so nothing here blocks on anything there.
struct SendDriver<P: Protocol> {
    transport: transport::AnySender,
    outgoing: mpsc::UnboundedReceiver<Outgoing<P::Response>>,
    shared: Arc<Shared>,
    scheduler: fragment::Scheduler,
    /// How much this driver still owes before it may stop; see
    /// [`crate::driver`].
    drain: DrainWatch,
}

impl<P: Protocol> SendDriver<P> {
    fn new(
        transport: transport::AnySender,
        outgoing: mpsc::UnboundedReceiver<Outgoing<P::Response>>,
        shared: Arc<Shared>,
        drain: DrainWatch,
    ) -> Self {
        let scheduler = fragment::Scheduler::new(&shared.limits, shared.payload_budget.clone());
        Self {
            transport,
            outgoing,
            shared,
            scheduler,
            drain,
        }
    }

    /// Runs until the drain signal says this driver owes nothing more.
    ///
    /// Three ways that happens, and they differ only in how much counts as
    /// owed:
    ///
    /// * [`Drain::Running`] — the channel closed. Every handle that could
    ///   queue work is gone, including the receive driver's, so no credit can
    ///   arrive either; finish what is already started.
    /// * [`Drain::Graceful`] — shutdown was requested. The receive half is
    ///   still running, so finish *everything*, quota-blocked sends included,
    ///   then remain available for control messages until that half ends.
    /// * [`Drain::Abrupt`] — the receive half is gone. Finish what is
    ///   already started and abandon the rest.
    ///
    /// A committed write is never abandoned in any of them: the scheduler is
    /// advanced to a fragment boundary before the loop can exit.
    async fn run(mut self) -> Result<(), Error> {
        // Holding a clone of `outgoing`'s sender half (the receive driver's,
        // or a `CallContext`'s) is what represents the ability to still get a
        // message in, so the channel closing — every clone gone — is one of
        // the terminal conditions in its own right. It is no longer the only
        // one: under a graceful drain the receive driver keeps its clone
        // precisely so it can keep servicing credit, and the drain signal is
        // what says nothing more will be admitted.
        let mut closed = false;
        loop {
            let mode = self.drain.mode();
            let done = match mode {
                Drain::Running => closed && !self.scheduler.has_work(),
                // Also requires the channel to be drained. A handler queues
                // its response and *then* completes, and completing is what
                // seals the drain — so at the instant the signal arrives the
                // last response may still be sitting in the channel, not yet
                // admitted to the scheduler, which would leave `has_pending`
                // reporting nothing to do.
                // The receive driver retains a sender until its transport
                // ends. Staying alive until the channel closes preserves the
                // send transport for rejection and control messages received
                // after the response drain first becomes quiescent.
                Drain::Graceful => closed && !self.scheduler.has_pending(),
                Drain::Abrupt => !self.scheduler.has_work(),
            };
            if done {
                return Ok(());
            }
            tokio::select! {
                message = self.outgoing.recv(), if !closed => {
                    let Some(message) = message else {
                        closed = true;
                        continue;
                    };
                    self.admit(message).await?;
                }
                // Cancel-safe (a `watch` registration), and re-evaluating the
                // terminal condition is the whole of the arm — the loop head
                // above does the work.
                _ = self.drain.changed() => {}
                // Not raced against anything — see the matching comment in
                // client.rs's writer loop. A dropped send future could leave a
                // committed partial fragment on the transport, or — on
                // transports whose writes are dispatched to a detached
                // background task — let an abandoned write complete arbitrarily
                // later, after the peer has already torn down its end.
                _ = self.scheduler.ready(), if self.scheduler.has_pending() => {
                    match self.scheduler.advance(&mut self.transport).await? {
                        fragment::AdvanceOutcome::None | fragment::AdvanceOutcome::Aborted(_) => {}
                        #[cfg(target_os = "macos")]
                        fragment::AdvanceOutcome::Escrow { id, fds, handles_done } => {
                            self.shared.escrow_sent(id, fds, handles_done);
                        }
                    }
                    // Flush anything sent by the scheduler
                    let _ = self.transport.flush().await;
                }
            }
        }
    }

    /// Admits one outgoing item to the fragment scheduler.
    async fn admit(&mut self, message: Outgoing<P::Response>) -> Result<(), Error> {
        match message {
            Outgoing::Response { id, value, trailer } => {
                let mut ledger = session::Ledger::default();
                let mut put_handles = session::SessionFrame {
                    inner: EncodeHandles::new(&self.transport, self.shared.max_handles()),
                    session: &self.shared.session,
                    ledger: &mut ledger,
                };
                let payload = match encode_payload(&value, &mut put_handles) {
                    Ok(payload) => payload,
                    Err(error) => {
                        drop(put_handles);
                        // Nothing reached the wire, so undo the gift increments
                        // rather than letting the ledger's drop commit them.
                        ledger.rescind();
                        return Err(error);
                    }
                };
                let handles = self.shared.finish_handles(id, put_handles.inner);
                self.scheduler
                    .admit_message(Kind::Response, id, payload, handles, trailer, ledger);
            }
            Outgoing::Error { id } => self.scheduler.admit_empty(Kind::Error, id),
            Outgoing::Cancel { id } => match self.scheduler.try_cancel_active(id) {
                fragment::AbortOutcome::NotActive => {}
                fragment::AbortOutcome::Discarded { started, .. } => {
                    if started {
                        self.scheduler.admit_abort(id);
                    }
                    if !started {
                        self.shared.discard_unsent_escrow(id);
                    }
                }
            },
            Outgoing::DiscardTrailer { id } => self.scheduler.admit_empty(Kind::Discard, id),
            Outgoing::PeerDiscarded { id } => {
                // The peer will never credit what it just threw away; see the
                // client's matching arm.
                self.shared.trailer_session.settle(id);
                self.scheduler.discard_active_trailer(id);
            }
            Outgoing::Ack { id } => self.scheduler.admit_empty(Kind::Ack, id),
            Outgoing::Release { id, count } => self.scheduler.admit_release(id, count),
            Outgoing::Credit { id, count } => self.scheduler.admit_credit(id, count),
            Outgoing::PayloadCredit { count } => self.scheduler.admit_payload_credit(count),
        }
        Ok(())
    }
}

/// Request-scoped services supplied to a server handler.
///
/// A context is not cloneable and must be consumed to send a response.
pub struct CallContext<P: Protocol> {
    id: u64,
    shared: Arc<Shared>,
    request_trailer: Option<TrailerRecv>,
    /// A strong sender, so a live handler keeps the writer's channel — and
    /// with it the connection — open until it has answered.
    outgoing: mpsc::UnboundedSender<Outgoing<P::Response>>,
    responded: bool,
    shutdown_on_respond: bool,
    /// This request's share of the payload quota, returned to the peer when
    /// this context is dropped — which is every path a call can end on,
    /// including a handler that never responds, one aborted by a peer
    /// cancellation, and one that panics.
    charge: Option<PayloadCharge>,
    marker: PhantomData<fn() -> P>,
}

impl<P: Protocol> CallContext<P> {
    /// Takes this request's raw-byte trailer, if present.
    ///
    /// The returned value implements [`AsyncRead`](tokio::io::AsyncRead).
    /// Dropping it stops local consumption and immediately tells the peer to
    /// stop sending, as does responding while the context still holds it.
    ///
    /// Taken rather than borrowed, so a handler may keep reading after it
    /// has responded. Paired with
    /// [`respond_with_trailer`](Self::respond_with_trailer) that gives a
    /// duplex byte pipe over one call: each direction is an independent
    /// stream that ends when its own end says so, and the call itself is
    /// complete as soon as the response head goes out. Neither direction
    /// holds a call slot after that, so the pipes are bounded by trailer
    /// credit rather than by `max_concurrent_calls` — and, as with a socket,
    /// nothing ties the two halves together: closing one does not close the
    /// other, and a peer that vanishes is noticed through the transport.
    pub fn trailer(&mut self) -> Option<TrailerRecv> {
        self.request_trailer.take()
    }

    /// Returns this request's raw-byte trailer in manual-credit mode.
    ///
    /// The consumer then owes the peer an explicit
    /// [`TrailerRecv::release`](crate::trailer::TrailerRecv::release) for
    /// every chunk it finishes with, instead of credit being returned on
    /// read. Use this when the bytes are being handed somewhere slower than
    /// this process, so that the peer's send rate follows the real drain
    /// rate; read [`release`](crate::trailer::TrailerRecv::release) first,
    /// since manual mode moves a deadlock rule into calling code.
    ///
    /// The mode is fixed here rather than switchable afterwards, so a
    /// trailer cannot be half auto-credited and half not. Taken rather than
    /// borrowed, exactly as in [`trailer`](Self::trailer).
    pub fn trailer_manual_credit(&mut self) -> Option<TrailerRecv> {
        let mut trailer = self.request_trailer.take()?;
        trailer.set_manual_credit();
        Some(trailer)
    }

    /// Sends a response without a trailer and consumes this call context.
    ///
    /// A request trailer this context still holds is discarded; one already
    /// taken by [`trailer`](Self::trailer) is untouched and stays readable.
    pub fn respond(mut self, response: P::Response) {
        drop(self.request_trailer.take());
        self.responded = true;
        self.shared
            .inner
            .lock()
            .unwrap()
            .outstanding
            .remove(&self.id);
        let _ = self.outgoing.send(Outgoing::Response {
            id: self.id,
            value: response,
            trailer: fragment::Trailer::None,
        });
        self.finish_shutdown();
    }

    /// Sends a response head and returns a writer for its raw-byte trailer.
    ///
    /// Call [`TrailerSend::finish`](crate::trailer::TrailerSend::finish), or
    /// asynchronously shut down the returned writer, to commit the trailer.
    /// Dropping it without finishing aborts the trailer. A request trailer
    /// this context still holds is discarded; one already taken by
    /// [`trailer`](Self::trailer) is untouched, which is what makes the two
    /// directions a duplex pipe.
    pub fn respond_with_trailer(mut self, response: P::Response) -> TrailerSend<()> {
        drop(self.request_trailer.take());
        let shared = SendShared::new(
            Kind::Response,
            self.id,
            &self.shared.limits,
            self.shared.trailer_session.clone(),
        );
        self.responded = true;
        self.shared
            .inner
            .lock()
            .unwrap()
            .outstanding
            .remove(&self.id);
        let _ = self.outgoing.send(Outgoing::Response {
            id: self.id,
            value: response,
            trailer: fragment::Trailer::Stream(shared.clone()),
        });
        self.finish_shutdown();
        TrailerSend::new(shared, ())
    }

    /// Returns this request's payload quota to the peer now, rather than when
    /// this context is dropped.
    ///
    /// The quota is charged for the whole call, so a handler that pends for a
    /// long time throttles the connection for as long as it pends — which is
    /// fine for the small payloads a long-poll usually carries, and is not
    /// for a large one. This is the escape hatch: finish with the request,
    /// drop whatever you decoded from it, then release. Nothing checks that
    /// you did the first two, and releasing while still holding the request's
    /// data merely makes the peer's accounting optimistic.
    ///
    /// Idempotent, and never required — dropping the context releases just
    /// the same.
    pub fn release_payload(&mut self) {
        self.charge = None;
    }

    /// Requests graceful shutdown after this handler sends its response.
    ///
    /// The server stops accepting requests once this context is consumed by
    /// [`respond`](Self::respond) or [`respond_with_trailer`](Self::respond_with_trailer),
    /// then lets already-running handlers finish.
    pub fn shutdown(&mut self) {
        self.shutdown_on_respond = true;
    }

    fn finish_shutdown(&self) {
        if self.shutdown_on_respond
            && let Some(shutdown) = self.shared.inner.lock().unwrap().shutdown.take()
        {
            let _ = shutdown.send(());
        }
    }

    /// Runs an operation that can observe request cancellation without dropping
    /// the handler itself.
    ///
    /// If the peer cancels while `operation` is running, its future is dropped
    /// and this method returns [`RequestCancelled`]. The handler regains the
    /// context and may perform cleanup or send an application-level response.
    /// Only one cancellation guard may be active at a time; nesting guards
    /// panics.
    pub async fn cancel_guard<T, F>(&mut self, operation: F) -> Result<T, RequestCancelled>
    where
        F: AsyncFnOnce(&mut CallContext<P>) -> T,
    {
        struct Reset {
            id: u64,
            shared: Arc<Shared>,
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                if let Some(cancel) = self
                    .shared
                    .inner
                    .lock()
                    .unwrap()
                    .outstanding
                    .get_mut(&self.id)
                {
                    cancel.signal = None;
                }
            }
        }
        let (signal, cancelled) = oneshot::channel();
        {
            let mut inner = self.shared.inner.lock().unwrap();
            let cancel = inner
                .outstanding
                .get_mut(&self.id)
                .expect("call context is not registered");
            assert!(cancel.signal.is_none(), "cancel guard is already active");
            cancel.signal = Some(signal);
        }
        let _reset = Reset {
            id: self.id,
            shared: self.shared.clone(),
        };
        let future = operation(&mut *self);
        tokio::pin!(future);
        tokio::select! {
            value = &mut future => Ok(value),
            result = cancelled => match result { Ok(()) => Err(RequestCancelled), Err(_) => Ok(future.await) },
        }
    }

    /// Register an opqaue handle.
    ///
    /// The underlying resource will be automatically dropped when both of
    /// the following hold:
    /// - It is no longer referenced by the client, or the server has unregistered it
    /// - All oustanding [`OpaqueGuard`]s have been dropped
    ///
    /// # Panics
    ///
    /// If a different concrete type has already been registered under
    /// `T::Marker` on this session.
    pub fn register<T: OpaqueResource>(&self, value: T) -> Gift<T::Marker> {
        self.shared.session.register(value)
    }

    /// Acquires a guard an opaque handle citation.
    ///
    /// Returns [`InvalidOpaque`] if the resource was unregistered while the peer
    /// still held a reference to it.
    ///
    /// # Panics
    ///
    /// If the handle was minted by a different session.
    pub fn acquire<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<OpaqueGuard<T>, InvalidOpaque> {
        self.shared.session.acquire(value)
    }

    /// Unregisters an opaque handle
    ///
    /// If no outstanding [`OpaqueGuard`]s existed, the resource is returned
    /// directly; otherwise, `None` is returned and the resource will be
    /// dropped with the last `OpaqueGuard`.  In either case, subsequent
    /// uses of [`Self::acquire`] will fail.  If the handle has already
    /// been unregistered, returns [`InvalidOpaque`].
    ///
    /// # Panics
    ///
    /// If the handle was minted by a different session.
    pub fn unregister<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<Option<T>, InvalidOpaque> {
        self.shared.session.unregister::<T>(value)
    }

    /// Unregisters an opaque handle if not busy
    ///
    /// The recoverable counterpart of [`unregister`](Self::unregister): if
    /// outstanding [`OpaqueGuard`]s exist, the handle is not unregistered,
    /// which is signaled by a `None` return value.
    ///
    /// # Panics
    ///
    /// If the handle was minted by a different session, as with
    /// [`acquire`](Self::acquire).
    pub fn try_unregister<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<Option<T>, InvalidOpaque> {
        self.shared.session.try_unregister::<T>(value)
    }
}

impl<P: Protocol> Drop for CallContext<P> {
    fn drop(&mut self) {
        if !self.responded {
            self.shared
                .inner
                .lock()
                .unwrap()
                .outstanding
                .remove(&self.id);
            let _ = self.outgoing.send(Outgoing::Error { id: self.id });
        }
    }
}

/// Indicates that a guarded operation was interrupted by request cancellation.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("request cancelled")]
pub struct RequestCancelled;

#[cfg(test)]
mod tests {
    use std::{future, time::Duration};

    use bytes::Bytes;
    use tokio::task::JoinHandle;

    use super::*;

    const APP_PROTOCOL: (&str, &[u16]) = ("test", &[1]);
    const FRAGMENT_SIZE: usize = 512;

    struct Test;

    impl Protocol for Test {
        type Request = u32;
        type Response = u32;
    }

    /// A peer that speaks the wire format directly, so a test can send what
    /// a real client's own accounting would never let it send.
    struct Peer {
        transport: transport::AnySender,
        scheduler: fragment::Scheduler,
        /// Unread: nothing these tests do makes the server say anything back.
        _receiver: transport::AnyReceiver,
    }

    impl Peer {
        /// Queues a whole request, small enough to arrive in one fragment
        /// and be dispatched the moment it lands.
        fn request(&mut self, id: u64, value: u32) {
            self.admit(Kind::Request, id, postcard::to_stdvec(&value).unwrap());
        }

        /// Queues a request far too large for one fragment, so it stays in
        /// the server's reassembler for as long as the test declines to send
        /// the rest of it. Its bytes are never decoded, so they need not be
        /// a valid request.
        fn partial_request(&mut self, id: u64) {
            self.admit(Kind::Request, id, vec![0; 64 * FRAGMENT_SIZE]);
        }

        /// Queues a fragment of a kind only a server may send.
        fn response(&mut self, id: u64) {
            self.admit(Kind::Response, id, postcard::to_stdvec(&0u32).unwrap());
        }

        fn admit(&mut self, kind: Kind, id: u64, payload: Vec<u8>) {
            self.scheduler.admit_message(
                kind,
                id,
                Bytes::from(payload),
                Default::default(),
                fragment::Trailer::None,
                session::Ledger::default(),
            );
        }

        /// Writes `count` fragments. The scheduler round-robins, so a count
        /// equal to the number of queued messages puts one fragment of each
        /// on the wire.
        async fn send(&mut self, count: usize) {
            for _ in 0..count {
                self.scheduler.advance(&mut self.transport).await.unwrap();
            }
        }
    }

    fn endpoint_pair() -> (
        (transport::AnySender, transport::AnyReceiver),
        (transport::AnySender, transport::AnyReceiver),
    ) {
        let (a_write, a_read) = tokio::io::duplex(4096);
        let (b_write, b_read) = tokio::io::duplex(4096);
        let (a_sender, _unused) = transport::generic_duplex(a_write);
        let (_unused, a_receiver) = transport::generic_duplex(b_read);
        let (b_sender, _unused) = transport::generic_duplex(b_write);
        let (_unused, b_receiver) = transport::generic_duplex(a_read);
        (
            (
                transport::AnySender::Generic(a_sender),
                transport::AnyReceiver::Generic(a_receiver),
            ),
            (
                transport::AnySender::Generic(b_sender),
                transport::AnyReceiver::Generic(b_receiver),
            ),
        )
    }

    /// Negotiates a real session, serves one end of it, and hands back the
    /// wire-level peer on the other.
    ///
    /// The handler never responds, so every call it is given stays
    /// outstanding until the session ends under it — which is what lets a
    /// test hold calls in one custodian while it fills the other.
    async fn hostile_session(
        max_concurrent_calls: usize,
    ) -> (
        Peer,
        JoinHandle<Result<(), Error>>,
        mpsc::UnboundedReceiver<()>,
    ) {
        hostile_session_with(Limits {
            max_concurrent_calls,
            max_fragment_size: FRAGMENT_SIZE,
            ..Limits::default()
        })
        .await
    }

    async fn hostile_session_with(
        limits: Limits,
    ) -> (
        Peer,
        JoinHandle<Result<(), Error>>,
        mpsc::UnboundedReceiver<()>,
    ) {
        let ((mut peer_sender, mut peer_receiver), (mut server_sender, mut server_receiver)) =
            endpoint_pair();
        let (peer, server) = tokio::join!(
            fragment::negotiate(
                &mut peer_sender,
                &mut peer_receiver,
                &limits,
                APP_PROTOCOL,
                None
            ),
            fragment::negotiate(
                &mut server_sender,
                &mut server_receiver,
                &limits,
                APP_PROTOCOL,
                None
            ),
        );
        let peer_limits = peer.unwrap().limits;
        let server =
            Server::<Test>::from_transport(server_sender, server_receiver, server.unwrap().limits);
        let (dispatched_tx, dispatched) = mpsc::unbounded_channel();
        let serve = tokio::spawn(server.serve(async move |_: CallContext<Test>, _: u32| {
            let _ = dispatched_tx.send(());
            future::pending::<()>().await
        }));
        (
            Peer {
                transport: peer_sender,
                // An unbounded budget, so the peer can send what its
                // negotiated quota would have stopped it sending. That is
                // the whole point of this harness: the server's checks are
                // backstops against a peer that ignores what it agreed to,
                // and a peer that honoured it could never reach them.
                scheduler: fragment::Scheduler::new(
                    &peer_limits,
                    Arc::new(PayloadBudget::new(usize::MAX)),
                ),
                _receiver: peer_receiver,
            },
            serve,
            dispatched,
        )
    }

    /// A server that fails to notice a violation wedges rather than fails,
    /// and which await it wedges on depends on the violation, so the bound
    /// goes around the whole test.
    async fn bounded<F: Future>(test: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), test)
            .await
            .expect("the server should have refused the peer by now")
    }

    /// A call that has only started arriving is charged to the same budget
    /// as one already dispatched. The reassembler counting it separately
    /// would let a peer hold twice the limit — and twice the reassembly
    /// memory the limit exists to bound.
    ///
    /// Filling the budget exactly, and seeing the second call dispatched
    /// anyway, is also what pins the comparison at `>` rather than `>=`.
    #[tokio::test]
    async fn a_call_still_arriving_counts_against_dispatched_ones() {
        bounded(async {
            let (mut peer, serve, mut dispatched) = hostile_session(2).await;
            peer.request(1, 7);
            peer.send(1).await;
            dispatched.recv().await.unwrap();
            peer.request(2, 8);
            peer.send(1).await;
            dispatched.recv().await.unwrap();

            // Two of two are dispatched and unanswered, so a third is over
            // the limit from its very first fragment.
            peer.partial_request(3);
            peer.send(1).await;

            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message == "too many concurrent calls"
            ));
        })
        .await;
    }

    /// The mirror image: a dispatched call is charged to the same budget as
    /// one still arriving, so the check at dispatch has to add the
    /// reassembler's count to its own.
    #[tokio::test]
    async fn a_dispatched_call_counts_against_ones_still_arriving() {
        bounded(async {
            let (mut peer, serve, mut dispatched) = hostile_session(2).await;
            peer.request(1, 7);
            peer.send(1).await;
            dispatched.recv().await.unwrap();

            // Two of two: one dispatched and unanswered, one still arriving.
            peer.partial_request(2);
            peer.send(1).await;

            peer.request(3, 9);
            peer.send(2).await;

            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message == "too many concurrent calls"
            ));
        })
        .await;
    }

    /// The arriving call is already in the count, so a limit of zero admits
    /// nothing at all.
    #[tokio::test]
    async fn a_zero_call_limit_refuses_the_first_request() {
        bounded(async {
            let (mut peer, serve, _dispatched) = hostile_session(0).await;
            peer.request(1, 7);
            peer.send(1).await;
            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message == "too many concurrent calls"
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn a_request_reusing_a_live_call_id_is_refused() {
        bounded(async {
            let (mut peer, serve, mut dispatched) = hostile_session(4).await;
            peer.request(1, 7);
            peer.send(1).await;
            dispatched.recv().await.unwrap();
            peer.request(1, 8);
            peer.send(1).await;
            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message == "duplicate active request id 1"
            ));
        })
        .await;
    }

    /// The gate in front of the reassembler: nobody calls a client, so a
    /// response arriving here answers nothing.
    #[tokio::test]
    async fn a_response_from_the_peer_is_refused() {
        bounded(async {
            let (mut peer, serve, _dispatched) = hostile_session(4).await;
            peer.response(1);
            peer.send(1).await;
            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message == "server received a Response fragment"
            ));
        })
        .await;
    }

    /// The attack `max_outstanding_payload` exists to close: open many
    /// messages and send one fragment of each, and `max_concurrent_calls`
    /// alone admits every one of them — `max_payload_size` bounds each
    /// message, and nothing bounds the sum. Here the call count is deliberately
    /// generous, so the only thing that can refuse this is the byte quota.
    #[tokio::test]
    async fn a_peer_that_ignores_its_payload_quota_is_refused() {
        bounded(async {
            let (mut peer, serve, _dispatched) = hostile_session_with(Limits {
                max_concurrent_calls: 64,
                max_fragment_size: FRAGMENT_SIZE,
                // Both, because negotiation raises the quota to at least the
                // per-message cap: a pool that could not carry one legal
                // message would be a configuration with no legal traffic.
                max_payload_size: 4 * FRAGMENT_SIZE,
                max_outstanding_payload: 4 * FRAGMENT_SIZE,
                ..Limits::default()
            })
            .await;

            for id in 1..=16 {
                peer.partial_request(id);
            }
            // One fragment of each, round-robin, until the sum of the
            // reassembly buffers passes the quota. Driven from its own task
            // because the server stops reading the moment it objects, and a
            // peer writing into a full pipe would otherwise block forever
            // instead of letting the assertion below run.
            let sending = tokio::spawn(async move { peer.send(16).await });

            assert!(matches!(
                serve.await.unwrap(),
                Err(Error::Protocol(message)) if message.contains("session payload quota")
            ));
            sending.abort();
        })
        .await;
    }
}
