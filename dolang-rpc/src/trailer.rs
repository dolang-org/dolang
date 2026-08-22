//! Streaming request or response byte trailers.

use std::{
    io::{self, IoSlice},
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
};

use bytes::{Buf, BufMut, BytesMut, buf::UninitSlice};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{
    Limits,
    fragment::Kind,
    fragment::{Flags, FragmentHeader},
    transport::{AnyRecv, AnySend, RecvFrame, SendFrame},
    window::{ControlSink, SessionWindow},
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn register_waker(slot: &mut Option<Waker>, waker: &Waker) {
    if !slot
        .as_ref()
        .is_some_and(|current| current.will_wake(waker))
    {
        *slot = Some(waker.clone());
    }
}

fn wake(waker: Option<Waker>) {
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendAction {
    Fragment,
    Finish,
    Abort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendState {
    /// Nothing staged, no fragment in flight, no token requested.
    Idle,
    /// The writer wants to send a fragment and has asked to be scheduled,
    /// but no token has been granted yet.
    Demand,
    /// A token was just granted in response to `Demand`. `wait_fragment` is
    /// giving the writer one grace period to show up and use the zero-copy
    /// fast path directly against the token.
    Granted,
    /// The grace period following `Granted` expired without the writer
    /// reappearing. The token is still held; the *next* `poll_write` stages
    /// directly into `buffer` instead of attempting zero-copy, and wakes
    /// the driver itself — no further grant/round-trip is needed.
    Staging,
    /// `buffer` holds a fragment (header + payload, or whatever's left of
    /// one) ready to flush.
    Fragment,
    /// `buffer` still holds an unflushed fragment *and* the writer has
    /// more data ready to stage as soon as it drains.
    FragmentDemand,
    Finish,
    FragmentFinish,
    /// A clean, local abort (peer discard, cancellation, or the producer
    /// dropping `TrailerSend` without finishing): the driver observes this
    /// as an ordinary `SendAction::Abort` (wire `ABORT` fragment, no
    /// connection-level failure), while the producer observes it as
    /// `SendShared::error` on its next `poll_write`/`poll_flush`. `state`
    /// is authoritative for whether `error` is set — always paired with it,
    /// never checked independently.
    Abort,
    /// A genuine I/O failure (mid-flush or mid zero-copy write). Both the
    /// producer and the driver observe this as `Err` (via `error`, same
    /// pairing as `Abort`); for the driver it propagates out of
    /// `wait_fragment` as connection-fatal.
    Failed,
}

pub(crate) struct SendShared {
    token: Option<AnySend<'static>>,
    kind: Kind,
    id: u64,
    max_fragment_size: usize,
    copy_threshold: usize,
    /// Total bytes committed to fragments so far (staged or written
    /// zero-copy), regardless of whether they've actually reached the wire
    /// yet.
    written: usize,
    /// Pool credit held for the fragment currently being formed but not yet
    /// committed. Nonzero only while a large write is waiting for its
    /// transport grant: the reservation is taken in `Idle` and carried across
    /// `Demand` so that reaching `Granted`/`Staging` always has credit in
    /// hand, and those states never have to park while holding a lease.
    reserved: usize,
    /// The connection-wide credit pool, and the only thing bounding what this
    /// trailer may have outstanding — there is no per-trailer window, see
    /// [`SessionWindow`]. Outstanding bytes are tracked there by message id,
    /// not here, so they can still be settled once this `SendShared` is gone.
    session: Arc<SessionWindow>,
    /// Unsent suffix of a committed fragment. While `poll_write` holds the
    /// mutex, this temporarily contains the header before it is committed.
    buffer: BytesMut,
    state: SendState,
    /// Set exactly when `state` is `Abort` or `Failed`, cleared never
    /// (states never revert). Only ever read once `state` has already
    /// established one of those two, so it doesn't need its own
    /// `is_some()`/`is_none()` check anywhere.
    error: Option<(io::ErrorKind, String)>,
    /// Whether the message carrying this trailer has been admitted to the
    /// scheduler's active set.
    ///
    /// A trailer cannot precede its own payload on the wire, so a producer
    /// writing into a message that has not started yet can make no progress
    /// whatever it does. Reserving credit anyway is worse than useless: it is
    /// a deadlock, because payload quota and trailer credit are exactly the
    /// two things such a message is waiting for. Several unstarted messages
    /// can between them reserve the whole trailer pool for fragments that
    /// cannot be sent, starving the started messages whose completion is the
    /// only thing that would release the payload quota they are waiting on.
    started: bool,
    writer_waker: Option<Waker>,
    driver_waker: Option<Waker>,
}

impl SendShared {
    pub(crate) fn new(
        kind: Kind,
        id: u64,
        limits: &Limits,
        session: Arc<SessionWindow>,
    ) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            token: None,
            kind,
            id,
            max_fragment_size: limits.max_fragment_size,
            copy_threshold: limits.trailer_send_copy_threshold,
            written: 0,
            reserved: 0,
            session,
            buffer: BytesMut::new(),
            state: SendState::Idle,
            error: None,
            started: false,
            writer_waker: None,
            driver_waker: None,
        }))
    }

    /// Announces that the message carrying this trailer has been admitted and
    /// will be driven, releasing any producer parked on that.
    pub(crate) fn start(shared: &Mutex<Self>) {
        let mut inner = lock(shared);
        if inner.started {
            return;
        }
        inner.started = true;
        let writer = inner.writer_waker.take();
        drop(inner);
        wake(writer);
    }

    pub(crate) fn poll_action(shared: &Mutex<Self>, cx: &mut Context<'_>) -> Poll<SendAction> {
        let mut inner = lock(shared);
        inner.driver_waker.take();
        match inner.state {
            SendState::Demand
            | SendState::Fragment
            | SendState::FragmentDemand
            | SendState::FragmentFinish => Poll::Ready(SendAction::Fragment),
            SendState::Finish => Poll::Ready(SendAction::Finish),
            SendState::Abort => Poll::Ready(SendAction::Abort),
            SendState::Idle | SendState::Granted | SendState::Staging => {
                register_waker(&mut inner.driver_waker, cx.waker());
                Poll::Pending
            }
            SendState::Failed => {
                // Defensive: `Failed` is only ever set while a lease from
                // `grant` is live (mid-flush, or by the producer's own
                // zero-copy write), and by the time it's live this
                // `ActiveSend` has already been popped out of the
                // scheduler's queue — so `poll_action` should never
                // observe it. Wait rather than treat it as unreachable.
                register_waker(&mut inner.driver_waker, cx.waker());
                Poll::Pending
            }
        }
    }

    /// Installs a frame token whose real borrow is retained by the returned
    /// lease. The token is only accessed while `inner` is locked.
    ///
    /// A fresh grant with nothing already staged (`buffer` empty — the
    /// ordinary case, `state` is `Demand`) starts the zero-copy grace period
    /// (`Granted`). A grant that arrives with `buffer` already non-empty
    /// (`Fragment`/`FragmentDemand`, data staged from an earlier lease)
    /// leaves `state` alone — there is nothing to wait for, `wait_fragment`
    /// should drain it immediately.
    pub(crate) unsafe fn grant<'a>(
        shared: &Arc<Mutex<Self>>,
        token: AnySend<'a>,
        max_fragment_size: usize,
    ) -> SendLease<'a> {
        // SAFETY: `SendLease` retains the source mutable borrow and clears the
        // token under the same mutex before that borrow ends.
        let token = unsafe { std::mem::transmute::<AnySend<'a>, AnySend<'static>>(token) };
        let mut inner = lock(shared);
        assert!(inner.token.is_none());
        if inner.buffer.is_empty() {
            inner.state = SendState::Granted;
        }
        inner.token = Some(token);
        inner.max_fragment_size = max_fragment_size;
        let writer = inner.writer_waker.take();
        drop(inner);
        wake(writer);
        SendLease {
            shared: shared.clone(),
            armed: true,
            _borrow: PhantomData,
        }
    }

    /// Waits for the next fragment/finish/abort decision, draining any
    /// bytes the writer couldn't hand the transport synchronously.
    ///
    /// While `Granted`, gives the writer one cooperative scheduling turn to
    /// show up and use the zero-copy fast path; if it doesn't, flips the
    /// state to `Staging` (still holding the token) and keeps waiting — the
    /// next `poll_write` will stage into `buffer` and wake this same wait
    /// directly, with no further grant needed.
    ///
    /// Returns, alongside the action, whether the fragment that just
    /// completed needed this draining at all (`false`) as opposed to having
    /// been written entirely within the writer's own `poll_write` call
    /// (`true`) — the same short-write signal `SendFrame::finish` reports,
    /// used by the scheduler to adapt fragment sizing.
    pub(crate) async fn wait_fragment(shared: &Mutex<Self>) -> io::Result<(SendAction, bool)> {
        let mut needed_drain = false;
        let mut yielded = false;
        loop {
            let outcome = std::future::poll_fn(|cx| {
                let mut inner = lock(shared);
                inner.driver_waker.take();
                if inner.state == SendState::Failed {
                    let (kind, message) = inner.error.clone().expect("error set for Failed");
                    return Poll::Ready(Err(io::Error::new(kind, message)));
                }
                if !inner.buffer.is_empty() {
                    needed_drain = true;
                    let result = poll_flush_buffer(&mut inner, cx);
                    match result {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            inner.state = SendState::Failed;
                            inner.error = Some((error.kind(), error.to_string()));
                            let writer = inner.writer_waker.take();
                            drop(inner);
                            wake(writer);
                            return Poll::Ready(Err(error));
                        }
                        Poll::Ready(Ok(())) => {}
                    }
                }
                let atomic = !needed_drain;
                match inner.state {
                    SendState::Fragment | SendState::FragmentDemand | SendState::FragmentFinish => {
                        Poll::Ready(Ok(Some((SendAction::Fragment, atomic))))
                    }
                    SendState::Finish => Poll::Ready(Ok(Some((SendAction::Finish, atomic)))),
                    SendState::Abort => Poll::Ready(Ok(Some((SendAction::Abort, atomic)))),
                    SendState::Granted if !yielded => {
                        // One cooperative scheduling turn for the writer to
                        // show up before we fall back to staging.
                        Poll::Ready(Ok(None))
                    }
                    SendState::Granted => {
                        inner.state = SendState::Staging;
                        register_waker(&mut inner.driver_waker, cx.waker());
                        Poll::Pending
                    }
                    SendState::Idle | SendState::Demand | SendState::Staging => {
                        // Defensive: none of these should be observable
                        // here (this function only runs while a lease from
                        // `grant` is live), but wait rather than treat it
                        // as unreachable.
                        register_waker(&mut inner.driver_waker, cx.waker());
                        Poll::Pending
                    }
                    SendState::Failed => unreachable!("handled above"),
                }
            })
            .await?;
            match outcome {
                Some(result) => return Ok(result),
                None => {
                    yielded = true;
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    fn poll_flush(shared: &Mutex<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = lock(shared);
        inner.writer_waker.take();
        match inner.state {
            SendState::Abort | SendState::Failed => {
                let (kind, message) = inner.error.clone().expect("error set for Abort/Failed");
                Poll::Ready(Err(io::Error::new(kind, message)))
            }
            _ if !inner.buffer.is_empty() => {
                register_waker(&mut inner.writer_waker, cx.waker());
                Poll::Pending
            }
            _ => Poll::Ready(Ok(())),
        }
    }

    fn finish(shared: &Mutex<Self>) {
        let mut inner = lock(shared);
        inner.state = match inner.state {
            SendState::Fragment | SendState::FragmentDemand => SendState::FragmentFinish,
            SendState::FragmentFinish => SendState::FragmentFinish,
            // Preserve an existing abort/failure verbatim rather than
            // silently discarding it — `poll_write`/`poll_flush` must keep
            // reporting the original error even if `finish` is (unusually)
            // still called afterward.
            aborted @ (SendState::Abort | SendState::Failed) => aborted,
            _ => SendState::Finish,
        };
        let driver = inner.driver_waker.take();
        let writer = inner.writer_waker.take();
        drop(inner);
        wake(driver);
        wake(writer);
    }

    /// The writer dropped its `TrailerSend` without finishing.
    fn abandon(shared: &Mutex<Self>) {
        Self::set_aborted(shared, io::ErrorKind::BrokenPipe, "trailer is closed");
    }

    /// Cuts a still-open trailer send short from the scheduler's side:
    /// records an error so the *live* `TrailerSend`'s writer observes a
    /// clean failure on its next write instead of hanging, waiting for a
    /// lease that will never come again. Used both for genuine cancellation
    /// and for a peer-issued `Discard` notice — the two differ only in
    /// whether the surrounding message as a whole is still considered
    /// valid, which is a concern for the caller, not for this shared state.
    /// Never observed by `wait_fragment`: both call sites remove or replace
    /// the `ActiveSend`'s trailer before the scheduler could poll this
    /// `SendShared` again.
    pub(crate) fn discard(shared: &Mutex<Self>) {
        Self::set_aborted(
            shared,
            io::ErrorKind::BrokenPipe,
            "trailer discarded by peer",
        );
    }

    fn set_aborted(shared: &Mutex<Self>, kind: io::ErrorKind, message: &str) {
        let mut inner = lock(shared);
        if !matches!(
            inner.state,
            SendState::Finish | SendState::FragmentFinish | SendState::Failed
        ) {
            inner.state = SendState::Abort;
            inner.error = Some((kind, message.into()));
        }
        let driver = inner.driver_waker.take();
        let writer = inner.writer_waker.take();
        let session = inner.session.clone();
        let id = inner.id;
        drop(inner);
        // No more credit will ever arrive for this trailer, so its share of
        // the pool has to come back here or it is lost until the connection
        // ends. `settle` is idempotent, so a `Credit` that crossed this on
        // the wire cannot double-refund.
        session.settle(id);
        wake(driver);
        wake(writer);
    }

    /// Reserves up to `want` bytes of pool credit, returning what was
    /// granted. Zero means the pool is empty and the caller must park.
    ///
    /// Peer-returned credit is applied to the pool directly by the endpoint,
    /// keyed by message id, rather than routed back through this trailer: a
    /// `Credit` may well arrive after this `SendShared` is gone — see
    /// [`SessionWindow`].
    fn reserve(&mut self, want: usize) -> usize {
        let granted = self.session.debit_up_to(self.id, want);
        self.written += granted;
        granted
    }

    /// Returns a reservation whose fragment was never committed.
    fn unreserve(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.written -= len;
        self.session.refund(self.id, len);
    }
}

/// Why `Granted`/`Staging` may assert they already hold a reservation: the
/// scheduler only grants after `poll_action` reports `Fragment`, which
/// `Idle` never does, so a fresh grant (the only kind that sets `Granted`,
/// since `grant` leaves a non-empty `buffer`'s state alone) is always
/// answering a `Demand` — and `Demand` is only entered from `Idle` with a
/// reservation in hand.
const RESERVED_HELD: &str = "Granted/Staging is only reached from Demand, which reserves";

/// Parks the writer because the session pool is empty, registering it there
/// so the matching refund wakes it.
///
/// This is only ever reached from `SendState::Idle`, so a starved
/// trailer simply reports `Pending` from `poll_action` and the scheduler
/// skips it. Parking in `Granted`/`Staging` instead would hold a live
/// `SendLease` while the driver waited forever, wedging the connection's
/// single writer.
fn park_for_credit(shared: &mut SendShared, cx: &mut Context<'_>) {
    // `writer_waker` covers abort and failure; the pool's own list covers the
    // credit this park is actually waiting for.
    register_waker(&mut shared.writer_waker, cx.waker());
    shared.session.park(cx.waker());
}

fn poll_flush_buffer(shared: &mut SendShared, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    let Some(token) = shared.token.as_mut() else {
        return Poll::Ready(Err(io::Error::other("send lease has no frame token")));
    };
    loop {
        if shared.buffer.is_empty() {
            break Poll::Ready(Ok(()));
        }
        match token.poll_write_once(cx, &shared.buffer) {
            Poll::Ready(Ok(0)) => break Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
            Poll::Ready(Ok(n)) => shared.buffer.advance(n),
            Poll::Ready(Err(error)) => break Poll::Ready(Err(error)),
            Poll::Pending => break Poll::Pending,
        }
    }
}

pub(crate) struct SendLease<'a> {
    shared: Arc<Mutex<SendShared>>,
    armed: bool,
    _borrow: PhantomData<&'a mut ()>,
}

impl SendLease<'_> {
    pub(crate) fn complete(mut self) {
        let mut shared = lock(&self.shared);
        shared.token.take();
        shared.buffer.clear();
        shared.state = match shared.state {
            SendState::Fragment | SendState::FragmentDemand => SendState::Idle,
            SendState::FragmentFinish => SendState::Finish,
            state => state,
        };
        let writer = shared.writer_waker.take();
        self.armed = false;
        drop(shared);
        wake(writer);
    }
}

impl Drop for SendLease<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut shared = lock(&self.shared);
        shared.token.take();
        shared.buffer = BytesMut::new();
        // Preserve an existing `Failed` (e.g. `wait_fragment` already
        // observed a real I/O failure and this lease is being dropped
        // uncompleted as a result) rather than downgrading it to a generic
        // revocation message.
        if shared.state != SendState::Failed {
            shared.state = SendState::Abort;
            if shared.error.is_none() {
                shared.error = Some((
                    io::ErrorKind::ConnectionAborted,
                    "send grant was revoked".into(),
                ));
            }
        }
        let writer = shared.writer_waker.take();
        shared.driver_waker.take();
        drop(shared);
        wake(writer);
    }
}

/// Trailer send handle.
///
/// Call [`finish`](Self::finish) when done sending data;
/// dropping it first aborts the trailer. `finish` returns
/// an associated operation handle, such as a [`Call`](crate::client::Call).
pub struct TrailerSend<T> {
    shared: Arc<Mutex<SendShared>>,
    completion: Option<T>,
}

impl<T> TrailerSend<T> {
    pub(crate) fn new(shared: Arc<Mutex<SendShared>>, completion: T) -> Self {
        Self {
            shared,
            completion: Some(completion),
        }
    }

    /// Commits the trailer and returns the associated handle.
    ///
    /// This does not wait for buffered trailer bytes to reach the peer. Use
    /// [`AsyncWriteExt::shutdown`](tokio::io::AsyncWriteExt::shutdown) first
    /// when that ordering matters to the caller.
    pub fn finish(mut self) -> T {
        SendShared::finish(&self.shared);
        self.completion.take().unwrap()
    }
}

impl<T: Unpin> AsyncWrite for TrailerSend<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        let mut inner = lock(&this.shared);
        inner.writer_waker.take();
        match inner.state {
            SendState::Finish | SendState::FragmentFinish => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "trailer is closed",
            ))),
            SendState::Abort | SendState::Failed => {
                let (kind, message) = inner.error.clone().expect("error set for Abort/Failed");
                Poll::Ready(Err(io::Error::new(kind, message)))
            }
            SendState::Fragment | SendState::FragmentDemand => {
                // A previously staged fragment hasn't been fully flushed
                // yet — wait for the driver to drain it before staging (or
                // writing) more. This is real backpressure (bounded to one
                // fragment's worth of staged data), not a wait for the
                // driver to win a scheduling race, so it's fine to block.
                inner.state = SendState::FragmentDemand;
                register_waker(&mut inner.writer_waker, cx.waker());
                let driver = inner.driver_waker.take();
                drop(inner);
                wake(driver);
                Poll::Pending
            }
            SendState::Idle => {
                // Nothing may be reserved before the message itself has been
                // admitted; see `SendShared::started`. Parking here rather
                // than after the reservation is the whole point.
                if !inner.started {
                    register_waker(&mut inner.writer_waker, cx.waker());
                    return Poll::Pending;
                }
                // The only place credit is ever waited for. Doing it here,
                // before demanding a token, is what keeps a starved trailer
                // from holding a transport grant hostage — see
                // `park_for_credit`. The reservation taken here is *held*
                // across `Demand`, so `Granted`/`Staging` always have credit
                // in hand and never have to park while holding a lease. That
                // matters now that the pool is the sole limiter: another
                // trailer could otherwise drain it in between.
                let want = buf.len().min(inner.max_fragment_size.max(1));
                let len = inner.reserve(want);
                if len == 0 {
                    park_for_credit(&mut inner, cx);
                    return Poll::Pending;
                }
                if len <= inner.copy_threshold {
                    FragmentHeader {
                        flags: Flags::NONE,
                        kind: inner.kind,
                        id: inner.id,
                        payload_len: len,
                    }
                    .encode_into(&mut inner.buffer);
                    inner.buffer.extend_from_slice(&buf[..len]);
                    inner.state = SendState::Fragment;
                    let driver = inner.driver_waker.take();
                    drop(inner);
                    wake(driver);
                    return Poll::Ready(Ok(len));
                }
                // A large write asks to be granted a token for direct I/O,
                // carrying its reservation along.
                inner.reserved = len;
                inner.state = SendState::Demand;
                register_waker(&mut inner.writer_waker, cx.waker());
                let driver = inner.driver_waker.take();
                drop(inner);
                wake(driver);
                Poll::Pending
            }
            SendState::Demand => {
                register_waker(&mut inner.writer_waker, cx.waker());
                Poll::Pending
            }
            SendState::Staging => {
                // The grace period for zero-copy already expired for this
                // grant: stage directly and wake the driver, which is
                // already waiting for exactly this.
                debug_assert!(inner.reserved > 0, "{RESERVED_HELD}");
                let len = buf
                    .len()
                    .min(inner.max_fragment_size.max(1))
                    .min(inner.reserved);
                let unused = inner.reserved - len;
                inner.unreserve(unused);
                inner.reserved = 0;
                FragmentHeader {
                    flags: Flags::NONE,
                    kind: inner.kind,
                    id: inner.id,
                    payload_len: len,
                }
                .encode_into(&mut inner.buffer);
                inner.buffer.extend_from_slice(&buf[..len]);
                inner.state = SendState::Fragment;
                let driver = inner.driver_waker.take();
                drop(inner);
                wake(driver);
                Poll::Ready(Ok(len))
            }
            SendState::Granted => {
                // Zero-copy fast path: a token is granted and waiting on
                // us, so try writing directly instead of staging.
                debug_assert!(inner.reserved > 0, "{RESERVED_HELD}");
                let len = buf
                    .len()
                    .min(inner.max_fragment_size.max(1))
                    .min(inner.reserved);
                // Release the part of the reservation this fragment won't
                // use, but keep `len` held: a `Pending` below retries against
                // it rather than re-reserving against a pool that may have
                // been drained meanwhile.
                let unused = inner.reserved - len;
                inner.unreserve(unused);
                inner.reserved = len;
                FragmentHeader {
                    flags: Flags::NONE,
                    kind: inner.kind,
                    id: inner.id,
                    payload_len: len,
                }
                .encode_into(&mut inner.buffer);

                let header_len = inner.buffer.len();
                let write_result = {
                    let shared = &mut *inner;
                    let bufs = [IoSlice::new(&shared.buffer), IoSlice::new(&buf[..len])];
                    shared
                        .token
                        .as_mut()
                        .expect("installed send token")
                        .poll_write_vectored_once(cx, &bufs)
                };
                match write_result {
                    Poll::Ready(Ok(0)) => {
                        let error = io::Error::from(io::ErrorKind::WriteZero);
                        inner.buffer.clear();
                        inner.unreserve(len);
                        inner.reserved = 0;
                        inner.state = SendState::Failed;
                        inner.error = Some((error.kind(), error.to_string()));
                        let driver = inner.driver_waker.take();
                        drop(inner);
                        wake(driver);
                        Poll::Ready(Err(error))
                    }
                    Poll::Ready(Ok(n)) => {
                        debug_assert!(n <= header_len + len);
                        if n < header_len {
                            inner.buffer.advance(n);
                            inner.buffer.extend_from_slice(&buf[..len]);
                        } else {
                            inner.buffer.clear();
                            inner.buffer.extend_from_slice(&buf[n - header_len..len]);
                        }
                        inner.reserved = 0;
                        inner.state = SendState::Fragment;
                        let driver = inner.driver_waker.take();
                        drop(inner);
                        wake(driver);
                        Poll::Ready(Ok(len))
                    }
                    Poll::Ready(Err(error)) => {
                        inner.buffer.clear();
                        inner.unreserve(len);
                        inner.reserved = 0;
                        inner.state = SendState::Failed;
                        inner.error = Some((error.kind(), error.to_string()));
                        let driver = inner.driver_waker.take();
                        drop(inner);
                        wake(driver);
                        Poll::Ready(Err(error))
                    }
                    Poll::Pending => {
                        // No header bytes were committed, so this write
                        // remains entirely the caller's and can be retried
                        // with a new buffer. The reservation stays held for
                        // that retry.
                        inner.buffer.clear();
                        Poll::Pending
                    }
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        SendShared::poll_flush(&self.get_mut().shared, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        SendShared::finish(&this.shared);
        SendShared::poll_flush(&this.shared, cx)
    }
}

impl<T> Drop for TrailerSend<T> {
    fn drop(&mut self) {
        if self.completion.is_some() {
            SendShared::abandon(&self.shared);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecvState {
    Idle,
    /// The consumer polled while no fragment was granted and is waiting for
    /// the next fragment specifically.
    Demand,
    /// A fragment is granted and a zero-copy read returned `Pending`, so the
    /// transport registered the consumer's waker. `wait_fragment` trusts
    /// that registration instead of taking over draining itself.
    Reading,
    /// A fragment is granted, but nothing yet guarantees the consumer will
    /// be polled again — either it hasn't asked for this fragment at all
    /// yet, or an earlier read happened to exactly satisfy the previous
    /// fragment and it never came back for this one. `wait_fragment` gives
    /// this state exactly one cooperative scheduling turn to resolve into
    /// `Reading` on its own before falling back to `Draining`.
    Unclaimed,
    /// The driver has taken over pulling the remainder of the fragment off
    /// the wire into `stage` — reached either from `Unclaimed` (grace
    /// turn passed uneventfully) or directly from `Reading`/`Unclaimed`
    /// when a consumer's own zero-copy read came up short and didn't ask
    /// for more. The consumer now only reads from `stage`.
    Draining,
    Fragment,
    /// The current fragment is complete, but its lease has not yet been
    /// released and the consumer has already polled for the next fragment.
    FragmentDemand,
    Eof,
    Discard,
    /// A read failed, or the grant/connection was aborted/revoked. `error`
    /// holds the `io::Error` to report; `state` is authoritative for
    /// whether it's set, never checked independently.
    Failed,
}

pub(crate) struct RecvShared {
    token: Option<AnyRecv<'static>>,
    remaining: usize,
    /// Bytes the driver pulled off the wire on the consumer's behalf while
    /// in `RecvState::Draining` (or discarded on the wire but not yet
    /// consumed via `RecvState::Discard`). Always drained to the consumer
    /// before anything else — see `TrailerRecv::poll_read`.
    stage: BytesMut,
    copy_threshold: usize,
    demand_copy_threshold: usize,
    state: RecvState,
    /// Set exactly when `state` is `Failed`, cleared never.
    error: Option<(io::ErrorKind, String)>,
    reader_waker: Option<Waker>,
    driver_waker: Option<Waker>,
    /// How much retired credit accumulates before a `Credit` goes out.
    /// Purely local coalescing granularity — what actually bounds the peer is
    /// the session pool, not this.
    credit_interval: usize,
    /// Cumulative trailer bytes accepted from the wire, and cumulative bytes
    /// retired (already credited, plus `pending`). Kept only to catch a
    /// manual consumer releasing more than it was given.
    received: usize,
    retired: usize,
    /// Retired but not yet sent as a `Credit`. See `retire`.
    pending: usize,
    /// When false, the consumer releases credit explicitly via
    /// [`TrailerRecv::release`] instead of implicitly on read.
    auto_release: bool,
    /// The route back to the peer, for the `Credit`/`Discard` fragments this
    /// trailer sends on its own behalf.
    sink: Arc<dyn ControlSink>,
    /// The message id those fragments name.
    id: u64,
    /// The receive-side session pool, so `accept_bytes` can check the
    /// aggregate bound and retirement can give back to it.
    session: Arc<SessionWindow>,
}

impl RecvShared {
    pub(crate) fn new(
        copy_threshold: usize,
        demand_copy_threshold: usize,
        credit_interval: usize,
        session: Arc<SessionWindow>,
        id: u64,
        sink: Arc<dyn ControlSink>,
    ) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            token: None,
            remaining: 0,
            stage: BytesMut::new(),
            copy_threshold,
            demand_copy_threshold,
            state: RecvState::Idle,
            error: None,
            reader_waker: None,
            driver_waker: None,
            credit_interval,
            received: 0,
            retired: 0,
            pending: 0,
            auto_release: true,
            sink,
            id,
            session,
        }))
    }

    /// Accounts `len` bytes arriving from the wire against the session pool,
    /// returning a reason if the peer overran the credit it was granted.
    ///
    /// This is the backstop that makes the memory bound hold against a peer
    /// that ignores the credit it agreed to; a well-behaved one never trips
    /// it. The pool is the only bound — there is no per-trailer window to
    /// check, because a sender over-committing one of its own trailers can
    /// only starve its other trailers, never this end.
    pub(crate) fn accept_bytes(shared: &Mutex<Self>, len: usize) -> Option<&'static str> {
        let mut inner = lock(shared);
        // A discarded trailer's bytes are sunk as they arrive and never
        // held, so the bound does not apply — there is nothing to bound, and
        // the peer is racing a `Discard` it has not seen yet.
        if inner.state == RecvState::Discard {
            return None;
        }
        if !inner.session.accept_bytes(inner.id, len) {
            return Some("exceeded the session trailer credit window");
        }
        inner.received += len;
        None
    }

    /// Records `len` bytes as retired and emits a `Credit` if that crosses
    /// the coalescing threshold.
    ///
    /// Emission happens at half a `credit_interval`, *or* whenever holding
    /// credit back could be what keeps the peer parked: the trailer has
    /// ended, the pool is exhausted, or this trailer's own consumer is
    /// blocked waiting for bytes that are not coming. None of those are
    /// optimisations — without them a consumer that retires less than the
    /// interval and then waits for more data deadlocks against a sender that
    /// is waiting for exactly the credit sitting in `pending`.
    fn retire(inner: &mut Self, len: usize) -> Option<(Arc<dyn ControlSink>, u64, u32)> {
        // `discard` already returned this trailer's whole outstanding debt
        // to the pool in one go. Bytes retired after that point — `poll_read`
        // still serves whatever was left in `stage`, and `discard` does not
        // consume the handle — must not be refunded a second time.
        if inner.state == RecvState::Discard {
            return None;
        }
        inner.pending += len;
        inner.retired += len;
        if inner.pending * 2 < inner.credit_interval && !Self::must_flush(inner) {
            return None;
        }
        Self::flush(inner)
    }

    /// Whether coalescing must give way, because credit held back here could
    /// be the only thing the peer is missing.
    ///
    /// The first two clauses are session-scoped: a finished trailer would
    /// otherwise strand its pool debt for the life of the connection, and a
    /// drained pool means the peer is provably parked. The third is
    /// trailer-scoped and is what lets a sender divide the pool among its own
    /// trailers however it likes: a private per-trailer budget is invisible
    /// from here, but a sender parked on one produces the same symptom as any
    /// other stall — this consumer asking for bytes that never arrive. Since
    /// `credit_interval` is a purely local knob neither end advertises, this
    /// is the only way a sender's subdivision can be safe without the two
    /// ends agreeing on a number.
    fn must_flush(inner: &Self) -> bool {
        inner.state == RecvState::Eof || inner.session.is_exhausted() || Self::is_stalled(inner)
    }

    /// Whether the consumer is parked with nothing left to read: it has
    /// demanded a fragment that has not arrived, and `stage` is empty.
    fn is_stalled(inner: &Self) -> bool {
        matches!(inner.state, RecvState::Demand | RecvState::FragmentDemand)
            && inner.stage.is_empty()
    }

    /// Emits whatever credit has accumulated, whatever the threshold says.
    fn flush(inner: &mut Self) -> Option<(Arc<dyn ControlSink>, u64, u32)> {
        if inner.state == RecvState::Discard || inner.pending == 0 {
            return None;
        }
        let count = u32::try_from(inner.pending).unwrap_or(u32::MAX) as usize;
        inner.pending -= count;
        inner.session.refund(inner.id, count);
        Some((inner.sink.clone(), inner.id, count as u32))
    }

    /// `retire` plus the send, for callers that hold the guard and can drop
    /// it first. The sink is never called under the mutex.
    fn retire_and_emit(shared: &Mutex<Self>, len: usize) {
        let mut inner = lock(shared);
        if !inner.auto_release {
            return;
        }
        let emit = Self::retire(&mut inner, len);
        drop(inner);
        if let Some((sink, id, count)) = emit {
            sink.credit(id, count);
        }
    }

    /// Installs a fresh fragment and selects copying or rendezvous according
    /// to whether the consumer demanded this specific fragment.
    pub(crate) unsafe fn grant<'a>(
        shared: &Arc<Mutex<Self>>,
        token: AnyRecv<'a>,
        remaining: usize,
    ) -> RecvLease<'a> {
        // SAFETY: `RecvLease` retains the source mutable borrow and clears
        // the token under the same mutex before that borrow ends.
        let token = unsafe { std::mem::transmute::<AnyRecv<'a>, AnyRecv<'static>>(token) };
        let mut inner = lock(shared);
        assert!(inner.token.is_none());
        if inner.state != RecvState::Discard {
            let demanded = inner.state == RecvState::Demand;
            let copy_threshold = if demanded {
                inner.demand_copy_threshold
            } else {
                inner.copy_threshold
            };
            inner.state = if remaining == 0 {
                if demanded {
                    RecvState::FragmentDemand
                } else {
                    RecvState::Fragment
                }
            } else if remaining <= copy_threshold {
                RecvState::Draining
            } else {
                RecvState::Unclaimed
            };
        }
        inner.token = Some(token);
        inner.remaining = remaining;
        let reader = if inner.state == RecvState::Unclaimed {
            inner.reader_waker.take()
        } else {
            None
        };
        drop(inner);
        wake(reader);
        RecvLease {
            shared: shared.clone(),
            armed: true,
            _borrow: PhantomData,
        }
    }

    /// Waits for the current fragment to be fully off the wire, driving the
    /// actual transport reads itself whenever the consumer isn't (state
    /// `Draining` or `Discard`) — see `TrailerRecv::poll_read` for how a
    /// consumer hands off to this. This is what guarantees forward
    /// progress independent of whether (or how promptly) the consumer
    /// polls: this function is only ever driven by the connection's single
    /// receiver loop, which is always being polled as long as the
    /// connection is alive.
    ///
    /// Returns `Ok(true)` if the trailer was discarded.
    pub(crate) async fn wait_fragment(shared: &Mutex<Self>) -> io::Result<bool> {
        // Persists across multiple polls of the single `poll_fn` future
        // below (for as long as this `wait_fragment` call remains
        // unresolved), same as a struct field would, but scoped to this
        // one grant with no separate reset needed.
        let mut grace_given = false;
        std::future::poll_fn(|cx| {
            let mut inner = lock(shared);
            inner.driver_waker.take();
            loop {
                match inner.state {
                    RecvState::Fragment | RecvState::FragmentDemand => {
                        return Poll::Ready(Ok(false));
                    }
                    RecvState::Discard if inner.remaining == 0 => return Poll::Ready(Ok(true)),
                    RecvState::Draining | RecvState::Discard => {}
                    RecvState::Reading => {
                        // Something already guarantees a future poll —
                        // trust it to make progress and call back.
                        register_waker(&mut inner.driver_waker, cx.waker());
                        return Poll::Pending;
                    }
                    RecvState::Unclaimed => {
                        if !grace_given {
                            // Nothing guarantees the consumer will call
                            // `poll_read` again (e.g. it hasn't asked for
                            // this fragment yet, or a buffered reader's
                            // read happened to land exactly on the previous
                            // fragment boundary). Give it one cooperative
                            // scheduling turn to show up on its own before
                            // taking over.
                            grace_given = true;
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        inner.state = RecvState::Draining;
                    }
                    RecvState::Idle | RecvState::Demand | RecvState::Eof => {
                        register_waker(&mut inner.driver_waker, cx.waker());
                        return Poll::Pending;
                    }
                    RecvState::Failed => {
                        // Defensive: `fail`/`RecvLease::Drop` only ever set
                        // this at a point where this function isn't
                        // concurrently driving the same `RecvShared` (see
                        // their doc comments), so this should be
                        // unreachable — wait rather than treat it as such.
                        register_waker(&mut inner.driver_waker, cx.waker());
                        return Poll::Pending;
                    }
                }
                let discard = inner.state == RecvState::Discard;
                let result = if discard {
                    let mut sink = [0u8; 8192];
                    let n = inner.remaining.min(sink.len());
                    let mut dest = &mut sink[..n];
                    inner
                        .token
                        .as_mut()
                        .expect("installed receive token")
                        .poll_read_once(cx, &mut dest)
                } else {
                    let remaining = inner.remaining;
                    inner.stage.reserve(remaining);
                    let RecvShared { token, stage, .. } = &mut *inner;
                    // `stage` may have more spare capacity than `remaining`
                    // left over from an earlier, larger fragment — cap the
                    // read so a transport with more already-buffered bytes
                    // available (e.g. the next fragment, already sitting in
                    // the OS receive buffer) can't overrun this fragment's
                    // boundary.
                    let mut limited = stage.limit(remaining);
                    token
                        .as_mut()
                        .expect("installed receive token")
                        .poll_read_once(cx, &mut limited)
                };
                match result {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    Poll::Ready(Ok(n)) => {
                        inner.remaining -= n;
                        if inner.remaining == 0 && inner.state == RecvState::Draining {
                            inner.state = RecvState::Fragment;
                        }
                        if !discard {
                            let reader = inner.reader_waker.take();
                            wake(reader);
                        }
                        // Loop back around: reassess state (may now be
                        // `Fragment`/still-zero `Discard`) or keep draining.
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => {
                        register_waker(&mut inner.driver_waker, cx.waker());
                        return Poll::Pending;
                    }
                }
            }
        })
        .await
    }

    /// Marks the trailer complete, flushing any credit coalescing was still
    /// holding back.
    ///
    /// That flush is not an optimisation. A trailer smaller than the
    /// coalescing threshold retires every one of its bytes below it, so
    /// nothing has been credited when it ends — and once it has ended
    /// nothing more can trigger a flush: no further bytes arrive, and a
    /// completed trailer sends no `Discard` for the peer to settle against.
    /// The peer's matching pool debt would then be stranded for the life of
    /// the connection, and a stream of small trailers would exhaust its
    /// session window and park it forever.
    pub(crate) fn finish(shared: &Mutex<Self>) {
        let mut inner = lock(shared);
        inner.state = RecvState::Eof;
        // Retiring nothing, purely to take the `Eof` flush path.
        let emit = Self::retire(&mut inner, 0);
        let reader = inner.reader_waker.take();
        drop(inner);
        wake(reader);
        if let Some((sink, id, count)) = emit {
            sink.credit(id, count);
        }
    }

    pub(crate) fn fail(shared: &Mutex<Self>, error: io::Error) {
        let mut inner = lock(shared);
        inner.state = RecvState::Failed;
        inner.error = Some((error.kind(), error.to_string()));
        let reader = inner.reader_waker.take();
        drop(inner);
        wake(reader);
    }

    /// Stops wanting this trailer, and tells the peer so immediately.
    ///
    /// The notice has to be eager. A sender parked on exhausted credit emits
    /// no further fragments, so the old trigger — noticing that another
    /// `TRAILER` fragment arrived unwanted — can never fire, and an
    /// abandoned trailer would leave its sender parked forever instead of
    /// aborting it. With manual release the ambiguity is worse still: "the
    /// consumer is holding bytes it has not released" and "the consumer is
    /// gone" look identical from the sender's side, and this is the only
    /// thing that distinguishes them.
    ///
    /// Idempotent, and safe after the trailer has already finished — a
    /// `Discard` for a completed trailer is a defined no-op on the peer.
    pub(crate) fn discard(shared: &Mutex<Self>) {
        let mut inner = lock(shared);
        // Entering `Discard` is what makes this idempotent: a consumer that
        // calls `discard()` and then drops the handle must not send two
        // notices, nor refund the pool twice.
        if inner.state == RecvState::Discard {
            return;
        }
        // A trailer that already ended has nothing to stop, so dropping its
        // handle — the overwhelmingly common case — stays silent. The pool
        // settlement below still runs.
        let ended = inner.state == RecvState::Eof;
        if !matches!(inner.state, RecvState::Eof | RecvState::Failed) {
            inner.state = RecvState::Discard;
        }
        let driver = inner.driver_waker.take();
        let notify = (!ended).then(|| (inner.sink.clone(), inner.id));
        // Whatever this trailer still owes the pool will never be retired
        // now, so hand it back in one go rather than stranding it for the
        // life of the connection.
        inner.pending = 0;
        inner.retired = inner.received;
        let session = inner.session.clone();
        let id = inner.id;
        drop(inner);
        session.settle(id);
        wake(driver);
        if let Some((sink, id)) = notify {
            sink.discard(id);
        }
    }
}

pub(crate) struct RecvLease<'a> {
    shared: Arc<Mutex<RecvShared>>,
    armed: bool,
    _borrow: PhantomData<&'a mut ()>,
}

impl RecvLease<'_> {
    pub(crate) fn complete(mut self) {
        let mut shared = lock(&self.shared);
        shared.token.take();
        shared.remaining = 0;
        shared.state = match shared.state {
            RecvState::Fragment => RecvState::Idle,
            RecvState::FragmentDemand => RecvState::Demand,
            state => state,
        };
        self.armed = false;
    }
}

impl Drop for RecvLease<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut shared = lock(&self.shared);
        shared.token.take();
        shared.remaining = 0;
        // Preserve an existing error (e.g. `fail` already ran for an
        // earlier ABORT on this message) rather than downgrading it to a
        // generic revocation message.
        shared.state = RecvState::Failed;
        if shared.error.is_none() {
            shared.error = Some((
                io::ErrorKind::ConnectionAborted,
                "receive grant was revoked".into(),
            ));
        }
        let reader = shared.reader_waker.take();
        shared.driver_waker.take();
        drop(shared);
        wake(reader);
    }
}

/// Trailer receive handle.
pub struct TrailerRecv {
    pub(crate) shared: Arc<Mutex<RecvShared>>,
}

impl TrailerRecv {
    pub(crate) fn new(shared: Arc<Mutex<RecvShared>>) -> Self {
        Self { shared }
    }

    /// Switches this trailer to manual credit release, before it is handed
    /// to the application.
    ///
    /// Not public: the mode is chosen where the trailer is obtained, so
    /// there is no way to switch one mid-stream and no half-credited state
    /// to reason about. See [`release`](Self::release) for the rules manual
    /// mode imposes on the consumer.
    pub(crate) fn set_manual_credit(&mut self) {
        let mut inner = lock(&self.shared);
        debug_assert!(
            inner.retired == 0,
            "manual credit must be selected before the first read"
        );
        inner.auto_release = false;
    }

    /// Returns `n` bytes of credit to the peer.
    ///
    /// Only meaningful on a trailer obtained in manual-credit mode — via
    /// [`CallContext::trailer_manual_credit`] or
    /// [`CallResult::into_response_trailer_manual_credit`].
    ///
    /// # Deadlocks
    ///
    /// Never attempt to receive multiple chunks or a fixed amount of data
    /// (e.g. via [`AsyncReadExt::read_exact`](::tokio::io::AsyncReadExt::read_exact))
    /// between credit releases, or a deadlock may result.
    ///
    /// # Panics
    ///
    /// In debug builds, if called on an automatic trailer, or if the running
    /// total exceeds the bytes actually delivered.
    ///
    /// [`CallContext::trailer_manual_credit`]: crate::server::CallContext::trailer_manual_credit
    /// [`CallResult::into_response_trailer_manual_credit`]: crate::client::CallResult::into_response_trailer_manual_credit
    pub fn release(&mut self, n: usize) {
        let mut inner = lock(&self.shared);
        debug_assert!(
            !inner.auto_release,
            "release requires a manual-credit trailer; credit is returned on read otherwise"
        );
        debug_assert!(
            inner.retired + n <= inner.received,
            "released more trailer credit than was delivered"
        );
        if n == 0 {
            return;
        }
        let emit = RecvShared::retire(&mut inner, n);
        drop(inner);
        if let Some((sink, id, count)) = emit {
            sink.credit(id, count);
        }
    }
}

impl AsyncRead for TrailerRecv {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut inner = lock(&this.shared);
        inner.reader_waker.take();
        // Bytes the driver already pulled off the wire on our behalf (see
        // `RecvState::Draining` below) always go out first, ahead of even a
        // recorded error or EOF — they were legitimately received and the
        // error/EOF is only discovered on the poll after they run out.
        //
        // Unlike `token`/`remaining`, whether `stage` has a backlog isn't
        // implied by `state`: the driver deliberately doesn't wait for us
        // to drain it before completing this lease and granting the next
        // fragment (that's the whole point of `Draining` — it lets the
        // driver keep pipelining ahead of a slow or absent reader), so
        // `state` can already describe a *later* fragment's grant while
        // `stage` still holds an *earlier* fragment's undelivered tail.
        // These are two genuinely independent facts, so `stage` has to be
        // checked directly rather than folded into `state`.
        if !inner.stage.is_empty() {
            let n = buf.remaining().min(inner.stage.len());
            buf.put_slice(&inner.stage[..n]);
            let _ = inner.stage.split_to(n);
            // One of the two points where bytes actually reach the
            // application, and so one of the two places auto-release retires
            // them. Drop the guard first: the sink is never called under the
            // mutex.
            drop(inner);
            RecvShared::retire_and_emit(&this.shared, n);
            return Poll::Ready(Ok(()));
        }
        match inner.state {
            RecvState::Failed => {
                let (kind, message) = inner.error.clone().expect("error set for Failed");
                Poll::Ready(Err(io::Error::new(kind, message)))
            }
            RecvState::Eof => Poll::Ready(Ok(())),
            RecvState::Idle | RecvState::Fragment => {
                inner.state = if inner.state == RecvState::Idle {
                    RecvState::Demand
                } else {
                    RecvState::FragmentDemand
                };
                register_waker(&mut inner.reader_waker, cx.waker());
                // The consumer has just gone from reading to waiting, with
                // nothing staged behind it. Coalescing has nothing left to
                // gain here and could cost everything: if the peer is parked
                // — on the pool, or on a per-trailer budget of its own that
                // this end cannot see — the credit sitting in `pending` is
                // exactly what would release it. `retire` covers credit
                // retired *while* stalled; this covers credit already
                // accumulated when the stall begins.
                let emit = RecvShared::flush(&mut inner);
                drop(inner);
                if let Some((sink, id, count)) = emit {
                    sink.credit(id, count);
                }
                Poll::Pending
            }
            RecvState::Demand | RecvState::FragmentDemand => {
                register_waker(&mut inner.reader_waker, cx.waker());
                Poll::Pending
            }
            RecvState::Draining | RecvState::Discard => {
                // The driver owns pulling bytes off the wire, so `stage`
                // (checked above) is the only thing we can serve from until
                // it is refilled or the fragment/trailer completes.
                register_waker(&mut inner.reader_waker, cx.waker());
                Poll::Pending
            }
            RecvState::Reading | RecvState::Unclaimed => {
                // Zero-copy path: read directly into the caller's buffer.
                // Both states guarantee a live token and `remaining > 0`
                // (set together by `grant`).
                let before = buf.filled().len();
                let mut adapter = ReadBufMut(buf);
                let mut limited = (&mut adapter).limit(inner.remaining);
                let result = inner
                    .token
                    .as_mut()
                    .expect("installed receive token")
                    .poll_read_once(cx, &mut limited);
                match result {
                    Poll::Ready(Ok(0)) => Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into())),
                    Poll::Ready(Ok(n)) => {
                        inner.remaining -= n;
                        if inner.remaining == 0 {
                            inner.state = RecvState::Fragment;
                        } else {
                            // A short read here says nothing about whether
                            // the consumer intends to ask for more soon — a
                            // buffered reader upstream (e.g. the zip
                            // crate's `BufReader`) routinely over-requests
                            // for read-ahead, then goes quiet once its
                            // immediate caller is satisfied, potentially
                            // forever. Hand the token off to the driver
                            // (`wait_fragment`), which is always being
                            // polled independent of this consumer and can
                            // therefore be relied on to finish draining the
                            // fragment into `stage` regardless. Leaving the
                            // remainder on the wire would instead stall the
                            // connection's single sequential reader — which
                            // must fully drain this fragment before it can
                            // read the *next* fragment, for any message —
                            // on a consumer poll that may never come,
                            // wedging every other in-flight call on the
                            // connection.
                            inner.state = RecvState::Draining;
                        }
                        let driver = inner.driver_waker.take();
                        drop(inner);
                        wake(driver);
                        // The other delivery point; see the `stage` branch.
                        RecvShared::retire_and_emit(&this.shared, n);
                        debug_assert_eq!(buf.filled().len() - before, n);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                    Poll::Pending => {
                        // The transport registered its own readiness waker
                        // for this task, so the consumer will be polled
                        // again once more data arrives — `wait_fragment`
                        // can trust that.
                        inner.state = RecvState::Reading;
                        Poll::Pending
                    }
                }
            }
        }
    }
}

impl Drop for TrailerRecv {
    fn drop(&mut self) {
        RecvShared::discard(&self.shared);
    }
}

struct ReadBufMut<'a, 'b>(&'a mut ReadBuf<'b>);

unsafe impl BufMut for ReadBufMut<'_, '_> {
    fn remaining_mut(&self) -> usize {
        self.0.remaining()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        // SAFETY: delegated to the caller of this unsafe method.
        unsafe { self.0.assume_init(cnt) };
        self.0.advance(cnt);
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        // SAFETY: `BufMut` exposes this region only as uninitialized storage.
        let unfilled = unsafe { self.0.unfilled_mut() };
        // SAFETY: `UninitSlice` has the same representation as a slice of
        // `MaybeUninit<u8>` and cannot initialize beyond this region.
        unsafe { UninitSlice::from_raw_parts_mut(unfilled.as_mut_ptr().cast(), unfilled.len()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{AnyReceiver, AnySender, Receiver, Sender, generic};

    /// Limits whose credit pool never binds, for tests about anything other
    /// than flow control.
    fn unbounded_limits() -> Limits {
        Limits {
            trailer_session_window: usize::MAX,
            ..zero_copy_limits()
        }
    }

    /// Limits under which every write is large enough to demand a token
    /// instead of staging, so a test can drive the zero-copy path with
    /// conveniently small buffers.
    fn zero_copy_limits() -> Limits {
        Limits {
            trailer_send_copy_threshold: 0,
            ..Limits::default()
        }
    }

    /// Drives a writer to the only state the scheduler ever grants from: a
    /// `Demand`, which is where the write's credit reservation is taken.
    /// Granting straight out of `Idle` would hand back a token the writer
    /// never asked for and skip the reservation with it.
    fn demand<T: Unpin>(trailer: &mut TrailerSend<T>, buf: &[u8]) {
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            Pin::new(trailer).poll_write(&mut cx, buf).is_pending(),
            "a write past the copy threshold must demand a token"
        );
    }

    fn send_shared(limits: Limits) -> Arc<Mutex<SendShared>> {
        send_shared_id(1, limits)
    }

    fn send_shared_id(id: u64, limits: Limits) -> Arc<Mutex<SendShared>> {
        let session = Arc::new(SessionWindow::new(limits.trailer_session_window));
        let shared = SendShared::new(Kind::Request, id, &limits, session);
        // These tests stand in for a message the scheduler has already
        // admitted; the unstarted case has its own test below.
        SendShared::start(&shared);
        shared
    }

    /// A `RecvShared` with a sink that records what it would have sent, so
    /// credit and discard decisions can be asserted directly.
    #[derive(Default)]
    struct RecordingSink {
        credits: Mutex<Vec<(u64, u32)>>,
        discards: Mutex<Vec<u64>>,
    }

    impl ControlSink for Arc<RecordingSink> {
        fn payload_credit(&self, _count: u32) {
            unreachable!("trailer tests never release payload quota")
        }

        fn credit(&self, id: u64, count: u32) {
            lock(&self.credits).push((id, count));
        }

        fn discard(&self, id: u64) {
            lock(&self.discards).push(id);
        }
    }

    fn recv_shared(limits: Limits) -> (Arc<Mutex<RecvShared>>, Arc<RecordingSink>) {
        let session = Arc::new(SessionWindow::new(limits.trailer_session_window));
        let sink = Arc::new(RecordingSink::default());
        let shared = RecvShared::new(
            limits.trailer_recv_copy_threshold,
            limits.trailer_recv_demand_copy_threshold,
            limits.trailer_credit_interval,
            session,
            7,
            Arc::new(sink.clone()),
        );
        (shared, sink)
    }

    fn poll_read_once(trailer: &mut TrailerRecv, output: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut read = ReadBuf::new(output);
        let mut cx = Context::from_waker(Waker::noop());
        match Pin::new(trailer).poll_read(&mut cx, &mut read) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read.filled().len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    struct CappedSink {
        bytes: Arc<Mutex<Vec<u8>>>,
        max_write: usize,
    }

    impl AsyncWrite for CappedSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let len = buf.len().min(self.max_write);
            lock(&self.bytes).extend_from_slice(&buf[..len]);
            Poll::Ready(Ok(len))
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let mut remaining = self.max_write;
            let mut written = 0;
            let mut output = lock(&self.bytes);
            for buf in bufs {
                let len = buf.len().min(remaining);
                output.extend_from_slice(&buf[..len]);
                written += len;
                remaining -= len;
                if remaining == 0 {
                    break;
                }
            }
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn small_send_stages_without_a_grant_and_large_send_demands_one() {
        let limits = Limits {
            max_fragment_size: 8,
            trailer_send_copy_threshold: 4,
            ..Limits::default()
        };

        let small_shared = send_shared(limits);
        let mut small = TrailerSend::new(small_shared.clone(), ());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut small).poll_write(&mut cx, b"data"),
            Poll::Ready(Ok(4))
        ));
        let header = FragmentHeader {
            flags: Flags::NONE,
            kind: Kind::Request,
            id: 1,
            payload_len: 4,
        }
        .encode();
        assert_eq!(
            &small_shared.lock().unwrap().buffer[..],
            [&header[..], b"data"].concat()
        );
        assert_eq!(small_shared.lock().unwrap().state, SendState::Fragment);

        let large_shared = send_shared(limits);
        let mut large = TrailerSend::new(large_shared.clone(), ());
        assert!(
            Pin::new(&mut large)
                .poll_write(&mut cx, b"large")
                .is_pending()
        );
        assert_eq!(large_shared.lock().unwrap().state, SendState::Demand);
        assert!(large_shared.lock().unwrap().buffer.is_empty());
    }

    #[test]
    fn receive_copy_threshold_depends_on_demand_for_this_fragment() {
        let undemanded = recv_shared(Limits {
            trailer_recv_copy_threshold: 1,
            trailer_recv_demand_copy_threshold: 4,
            ..unbounded_limits()
        })
        .0;
        let (_, receiver) = generic(tokio::io::empty(), tokio::io::sink());
        let mut receiver = AnyReceiver::Generic(receiver);
        let lease = unsafe { RecvShared::grant(&undemanded, receiver.recv(), 4) };
        assert_eq!(undemanded.lock().unwrap().state, RecvState::Unclaimed);
        drop(lease);

        let demanded = recv_shared(Limits {
            trailer_recv_copy_threshold: 1,
            trailer_recv_demand_copy_threshold: 4,
            ..unbounded_limits()
        })
        .0;
        let mut trailer = TrailerRecv::new(demanded.clone());
        let mut output = [0; 4];
        assert!(poll_read_once(&mut trailer, &mut output).is_pending());
        assert_eq!(demanded.lock().unwrap().state, RecvState::Demand);
        let (_, receiver) = generic(tokio::io::empty(), tokio::io::sink());
        let mut receiver = AnyReceiver::Generic(receiver);
        let lease = unsafe { RecvShared::grant(&demanded, receiver.recv(), 4) };
        assert_eq!(demanded.lock().unwrap().state, RecvState::Draining);
        drop(lease);
    }

    #[test]
    fn demand_at_a_completed_fragment_boundary_applies_to_the_next_fragment() {
        let shared = recv_shared(Limits {
            trailer_recv_copy_threshold: 0,
            trailer_recv_demand_copy_threshold: 0,
            ..unbounded_limits()
        })
        .0;
        let mut trailer = TrailerRecv::new(shared.clone());
        let (_, receiver) = generic(tokio::io::empty(), tokio::io::sink());
        let mut receiver = AnyReceiver::Generic(receiver);
        let lease = unsafe { RecvShared::grant(&shared, receiver.recv(), 0) };
        assert_eq!(shared.lock().unwrap().state, RecvState::Fragment);

        let mut output = [0; 1];
        assert!(poll_read_once(&mut trailer, &mut output).is_pending());
        assert_eq!(shared.lock().unwrap().state, RecvState::FragmentDemand);
        lease.complete();
        assert_eq!(shared.lock().unwrap().state, RecvState::Demand);
    }

    #[tokio::test]
    async fn unclaimed_large_receive_falls_back_to_driver_draining() {
        use tokio::io::AsyncWriteExt;

        let shared = recv_shared(Limits {
            trailer_recv_copy_threshold: 0,
            trailer_recv_demand_copy_threshold: 0,
            ..unbounded_limits()
        })
        .0;
        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(b"data").await.unwrap();
        let (_, receiver) = generic(reader, tokio::io::sink());
        let mut receiver = AnyReceiver::Generic(receiver);
        let lease = unsafe { RecvShared::grant(&shared, receiver.recv(), 4) };
        assert_eq!(shared.lock().unwrap().state, RecvState::Unclaimed);

        assert!(!RecvShared::wait_fragment(&shared).await.unwrap());
        assert_eq!(shared.lock().unwrap().state, RecvState::Fragment);
        assert_eq!(&shared.lock().unwrap().stage[..], b"data");
        lease.complete();
    }

    #[tokio::test]
    async fn demanded_large_receive_can_claim_the_grant_directly() {
        use tokio::io::AsyncWriteExt;

        let shared = recv_shared(Limits {
            trailer_recv_copy_threshold: 0,
            trailer_recv_demand_copy_threshold: 0,
            ..unbounded_limits()
        })
        .0;
        let mut trailer = TrailerRecv::new(shared.clone());
        let mut output = [0; 4];
        assert!(poll_read_once(&mut trailer, &mut output).is_pending());

        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(b"data").await.unwrap();
        let (_, receiver) = generic(reader, tokio::io::sink());
        let mut receiver = AnyReceiver::Generic(receiver);
        let lease = unsafe { RecvShared::grant(&shared, receiver.recv(), 4) };
        assert_eq!(shared.lock().unwrap().state, RecvState::Unclaimed);
        assert!(matches!(
            poll_read_once(&mut trailer, &mut output),
            Poll::Ready(Ok(4))
        ));
        assert_eq!(&output, b"data");
        assert!(!RecvShared::wait_fragment(&shared).await.unwrap());
        lease.complete();
        assert_eq!(shared.lock().unwrap().state, RecvState::Idle);
    }

    #[tokio::test]
    async fn abandoned_fragment_flushes_only_its_real_staged_suffix() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (sender, _) = generic(
            tokio::io::empty(),
            CappedSink {
                bytes: output.clone(),
                max_write: 16,
            },
        );
        let mut sender = AnySender::Generic(sender);
        let shared = send_shared(unbounded_limits());
        let data = (0..100).map(|value| value as u8).collect::<Vec<_>>();
        let mut trailer = TrailerSend::new(shared.clone(), ());
        demand(&mut trailer, &data);
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };

        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, &data))
            .await
            .unwrap();
        assert_eq!(written, data.len());
        {
            let inner = lock(&shared);
            let header_len = FragmentHeader {
                flags: Flags::NONE,
                kind: Kind::Request,
                id: 1,
                payload_len: data.len(),
            }
            .encode()
            .len();
            assert_eq!(&inner.buffer[..], &data[16 - header_len..]);
        }

        drop(trailer);
        assert_eq!(
            SendShared::wait_fragment(&shared).await.unwrap().0,
            SendAction::Abort
        );
        let header_len = FragmentHeader {
            flags: Flags::NONE,
            kind: Kind::Request,
            id: 1,
            payload_len: data.len(),
        }
        .encode()
        .len();
        assert_eq!(&lock(&output)[header_len..], data);
        lease.complete();
    }

    #[tokio::test]
    async fn partial_header_and_payload_share_the_stage_buffer() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (sender, _) = generic(
            tokio::io::empty(),
            CappedSink {
                bytes: output.clone(),
                max_write: 5,
            },
        );
        let mut sender = AnySender::Generic(sender);
        let shared = send_shared_id(7, unbounded_limits());
        let data = (0..32).map(|value| value as u8).collect::<Vec<_>>();
        let mut trailer = TrailerSend::new(shared.clone(), ());
        demand(&mut trailer, &data);
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };

        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, &data))
            .await
            .unwrap();
        assert_eq!(written, data.len());

        let header = FragmentHeader {
            flags: Flags::NONE,
            kind: Kind::Request,
            id: 7,
            payload_len: data.len(),
        }
        .encode();
        let mut expected_stage = Vec::from(&header[5..]);
        expected_stage.extend_from_slice(&data);
        assert_eq!(&lock(&shared).buffer[..], expected_stage);

        assert_eq!(
            SendShared::wait_fragment(&shared).await.unwrap().0,
            SendAction::Fragment
        );
        assert_eq!(&lock(&output)[..], [&header[..], &data].concat());
        lease.complete();
    }

    #[tokio::test]
    async fn finish_releases_an_unused_live_grant() {
        let (sender, _) = generic(tokio::io::empty(), tokio::io::sink());
        let mut sender = AnySender::Generic(sender);
        let shared = send_shared(unbounded_limits());
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };

        TrailerSend::new(shared.clone(), ()).finish();
        assert_eq!(
            SendShared::wait_fragment(&shared).await.unwrap().0,
            SendAction::Finish
        );
        lease.complete();
    }

    /// Exhausting the pool parks the writer instead of failing it. This is
    /// the whole behavioural change from the old `max_trailer_size` abort: a
    /// slow consumer throttles the sender, it does not break it.
    /// A trailer whose message has not been admitted yet must reserve
    /// nothing. It cannot send what it stages — a trailer fragment can never
    /// precede its own payload — and the credit it took would be held against
    /// the trailers that *can* send, whose completion is the only thing that
    /// would free the payload quota the unadmitted message is waiting for.
    #[tokio::test]
    async fn an_unstarted_trailer_reserves_no_credit() {
        let session = Arc::new(SessionWindow::new(64));
        // Ordinary staging limits, so the write after `start` completes
        // against the buffer rather than demanding a transport grant this
        // test has no driver to supply.
        let shared = SendShared::new(Kind::Request, 1, &Limits::default(), session.clone());
        let mut trailer = TrailerSend::new(shared.clone(), ());

        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut trailer).poll_write(&mut cx, b"abcd"),
            Poll::Pending
        ));
        assert_eq!(session.available(), 64, "nothing may be spent yet");
        assert_eq!(lock(&shared).state, SendState::Idle);

        SendShared::start(&shared);
        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"abcd"))
            .await
            .unwrap();
        assert_eq!(written, 4);
        assert_eq!(session.available(), 60);
    }

    #[tokio::test]
    async fn exhausted_credit_parks_the_writer_until_credit_arrives() {
        let (sender, _) = generic(tokio::io::empty(), tokio::io::sink());
        let mut sender = AnySender::Generic(sender);
        let session = Arc::new(SessionWindow::new(4));
        let shared = SendShared::new(Kind::Request, 1, &zero_copy_limits(), session.clone());
        SendShared::start(&shared);
        let mut trailer = TrailerSend::new(shared.clone(), ());

        // Spend the whole 4-byte pool.
        demand(&mut trailer, b"abcd");
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };
        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"abcd"))
            .await
            .unwrap();
        assert_eq!(written, 4);
        let (action, _) = SendShared::wait_fragment(&shared).await.unwrap();
        assert_eq!(action, SendAction::Fragment);
        lease.complete();

        // With nothing left, the next write parks rather than erroring, and
        // stays in `Idle` so it holds no transport grant while parked.
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut trailer).poll_write(&mut cx, b"e"),
            Poll::Pending
        ));
        assert_eq!(lock(&shared).state, SendState::Idle);
        assert!(matches!(
            SendShared::poll_action(&shared, &mut cx),
            Poll::Pending
        ));

        // Credit from the peer unblocks exactly that much more.
        session.refund(1, 2);
        demand(&mut trailer, b"ef");
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };
        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"ef"))
            .await
            .unwrap();
        assert_eq!(written, 2);
        lease.complete();
    }

    /// A credit pool smaller than a fragment is legal: it just produces
    /// short fragments. Only a zero pool would deadlock, and negotiation
    /// floors it at 1.
    #[tokio::test]
    async fn pool_below_fragment_size_still_makes_progress() {
        let (sender, _) = generic(tokio::io::empty(), tokio::io::sink());
        let mut sender = AnySender::Generic(sender);
        let session = Arc::new(SessionWindow::new(3));
        let limits = Limits {
            max_fragment_size: 1024,
            ..zero_copy_limits()
        };
        let shared = SendShared::new(Kind::Request, 1, &limits, session.clone());
        SendShared::start(&shared);
        let mut trailer = TrailerSend::new(shared.clone(), ());
        let mut total = 0;
        for _ in 0..4 {
            demand(&mut trailer, b"abcdefghij");
            let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };
            let n = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"abcdefghij"))
                .await
                .unwrap();
            assert_eq!(n, 3, "each write is clamped to the pool, not dropped");
            total += n;
            SendShared::wait_fragment(&shared).await.unwrap();
            lease.complete();
            session.refund(1, 3);
        }
        assert_eq!(total, 12);
    }

    /// The pool is the only limiter, so one trailer can exhaust it and park
    /// another. The parked writer holds no transport grant while it waits,
    /// and it is the *other* trailer's consumer retiring bytes that releases
    /// it — the cross-trailer coupling that comes with a single pool.
    #[tokio::test]
    async fn one_trailer_exhausting_the_pool_parks_another() {
        let (sender, _) = generic(tokio::io::empty(), tokio::io::sink());
        let mut sender = AnySender::Generic(sender);
        let session = Arc::new(SessionWindow::new(4));
        let limits = zero_copy_limits();
        let first = SendShared::new(Kind::Request, 1, &limits, session.clone());
        let second = SendShared::new(Kind::Request, 2, &limits, session.clone());
        SendShared::start(&first);
        SendShared::start(&second);

        let mut trailer = TrailerSend::new(first.clone(), ());
        demand(&mut trailer, b"abcdefgh");
        let lease = unsafe { SendShared::grant(&first, sender.send(), 1024) };
        let written = std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"abcdefgh"))
            .await
            .unwrap();
        assert_eq!(written, 4, "clamped by the pool");
        SendShared::wait_fragment(&first).await.unwrap();
        lease.complete();

        let mut other = TrailerSend::new(second.clone(), ());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut other).poll_write(&mut cx, b"i"),
            Poll::Pending
        ));
        assert_eq!(
            lock(&second).state,
            SendState::Idle,
            "a parked writer must hold no transport grant"
        );
        assert!(matches!(
            SendShared::poll_action(&second, &mut cx),
            Poll::Pending
        ));

        session.refund(1, 2);
        demand(&mut other, b"ij");
        let lease = unsafe { SendShared::grant(&second, sender.send(), 1024) };
        let written = std::future::poll_fn(|cx| Pin::new(&mut other).poll_write(cx, b"ij"))
            .await
            .unwrap();
        assert_eq!(written, 2);
        lease.complete();
    }

    /// Aborting a trailer must return its share of the pool, or a discarded
    /// transfer would shrink the connection's capacity permanently.
    #[tokio::test]
    async fn aborting_a_trailer_returns_its_session_debt() {
        let (sender, _) = generic(tokio::io::empty(), tokio::io::sink());
        let mut sender = AnySender::Generic(sender);
        let session = Arc::new(SessionWindow::new(64));
        let shared = SendShared::new(Kind::Request, 1, &zero_copy_limits(), session.clone());
        SendShared::start(&shared);
        let mut trailer = TrailerSend::new(shared.clone(), ());
        demand(&mut trailer, b"abcdefgh");
        let lease = unsafe { SendShared::grant(&shared, sender.send(), 1024) };
        std::future::poll_fn(|cx| Pin::new(&mut trailer).poll_write(cx, b"abcdefgh"))
            .await
            .unwrap();
        SendShared::wait_fragment(&shared).await.unwrap();
        lease.complete();
        assert_eq!(session.available(), 56);

        SendShared::discard(&shared);
        assert_eq!(session.available(), 64, "the pool is made whole again");
    }

    /// Auto-release is the default and needs no participation: bytes are
    /// retired as they reach the application.
    #[test]
    fn auto_release_credits_on_delivery() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 8,
            ..Limits::default()
        });
        RecvShared::accept_bytes(&shared, 8);
        lock(&shared).stage.extend_from_slice(b"abcdefgh");
        let mut trailer = TrailerRecv::new(shared.clone());

        let mut out = [0u8; 8];
        assert!(matches!(
            poll_read_once(&mut trailer, &mut out),
            Poll::Ready(Ok(8))
        ));
        assert_eq!(&*lock(&sink.credits), &[(7, 8)]);
    }

    /// In manual mode reading retires nothing; only `release` does.
    #[test]
    fn manual_release_does_not_credit_on_read() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 8,
            ..Limits::default()
        });
        RecvShared::accept_bytes(&shared, 8);
        lock(&shared).stage.extend_from_slice(b"abcdefgh");
        let mut trailer = TrailerRecv::new(shared.clone());
        trailer.set_manual_credit();

        let mut out = [0u8; 8];
        assert!(matches!(
            poll_read_once(&mut trailer, &mut out),
            Poll::Ready(Ok(8))
        ));
        assert!(lock(&sink.credits).is_empty(), "reading must not credit");

        trailer.release(8);
        assert_eq!(&*lock(&sink.credits), &[(7, 8)]);
    }

    /// Credit is coalesced at half an interval, so a chunk below that does
    /// not put a fragment on the wire by itself.
    #[test]
    fn credit_is_coalesced_below_half_an_interval() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 64,
            trailer_session_window: 1024,
            ..Limits::default()
        });
        RecvShared::accept_bytes(&shared, 32);
        let mut trailer = TrailerRecv::new(shared.clone());
        trailer.set_manual_credit();

        trailer.release(8);
        assert!(lock(&sink.credits).is_empty());
        trailer.release(8);
        assert!(lock(&sink.credits).is_empty());
        // Crossing half the interval (32) flushes the accumulated total as
        // one fragment rather than three.
        trailer.release(16);
        assert_eq!(&*lock(&sink.credits), &[(7, 32)]);
    }

    /// The anti-deadlock half of the coalescing rule: when the peer has
    /// provably run out of credit, even a sub-threshold release must go out,
    /// or both ends wait on each other forever.
    #[test]
    fn credit_is_flushed_when_the_peer_is_starved() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 64,
            trailer_session_window: 64,
            ..Limits::default()
        });
        // The peer has spent the entire pool and is certainly parked.
        assert_eq!(RecvShared::accept_bytes(&shared, 64), None);
        let mut trailer = TrailerRecv::new(shared.clone());
        trailer.set_manual_credit();

        trailer.release(1);
        assert_eq!(
            &*lock(&sink.credits),
            &[(7, 1)],
            "a starved peer must be credited immediately, however little"
        );
    }

    /// A sub-threshold chunk that accumulated while the consumer was reading
    /// must go out the moment the consumer starts waiting, or a sender parked
    /// on a per-trailer budget of its own — which this end cannot see — never
    /// learns it may continue.
    #[test]
    fn credit_is_flushed_when_the_consumer_starts_waiting() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 64,
            trailer_session_window: 1024,
            ..Limits::default()
        });
        RecvShared::accept_bytes(&shared, 8);
        lock(&shared).stage.extend_from_slice(b"abcdefgh");
        let mut trailer = TrailerRecv::new(shared.clone());

        let mut out = [0u8; 8];
        assert!(matches!(
            poll_read_once(&mut trailer, &mut out),
            Poll::Ready(Ok(8))
        ));
        assert!(
            lock(&sink.credits).is_empty(),
            "8 bytes is below half the interval, so it coalesces while reading"
        );

        // Nothing staged and nothing granted: the consumer is now blocked on
        // the peer, and coalescing has nothing left to gain.
        assert!(poll_read_once(&mut trailer, &mut out).is_pending());
        assert_eq!(&*lock(&sink.credits), &[(7, 8)]);
    }

    /// The other edge of the same rule: credit retired *while* the consumer
    /// is already waiting — where a manual consumer's `release` lands, since
    /// it runs on its own timeline — must not be held back either.
    #[test]
    fn credit_released_while_waiting_is_flushed_immediately() {
        let (shared, sink) = recv_shared(Limits {
            trailer_credit_interval: 64,
            trailer_session_window: 1024,
            ..Limits::default()
        });
        RecvShared::accept_bytes(&shared, 8);
        lock(&shared).stage.extend_from_slice(b"abcdefgh");
        let mut trailer = TrailerRecv::new(shared.clone());
        trailer.set_manual_credit();

        let mut out = [0u8; 8];
        assert!(matches!(
            poll_read_once(&mut trailer, &mut out),
            Poll::Ready(Ok(8))
        ));
        assert!(poll_read_once(&mut trailer, &mut out).is_pending());
        assert!(
            lock(&sink.credits).is_empty(),
            "manual mode retires nothing on read, so the stall flushes nothing"
        );

        trailer.release(8);
        assert_eq!(
            &*lock(&sink.credits),
            &[(7, 8)],
            "a waiting consumer's release goes out below the threshold"
        );
    }

    /// The starvation flush is session-scoped, and that is what replaces the
    /// per-trailer starvation check a two-window scheme would have had: a
    /// trailer retiring a sub-threshold chunk must still credit it when the
    /// pool was drained by some *other* trailer, or the parked sender never
    /// learns there is room.
    #[test]
    fn credit_is_flushed_when_another_trailer_drained_the_pool() {
        let limits = Limits {
            trailer_credit_interval: 1024,
            trailer_session_window: 64,
            ..Limits::default()
        };
        let session = Arc::new(SessionWindow::new(limits.trailer_session_window));
        let make = |id| {
            let sink = Arc::new(RecordingSink::default());
            let shared = RecvShared::new(
                limits.trailer_recv_copy_threshold,
                limits.trailer_recv_demand_copy_threshold,
                limits.trailer_credit_interval,
                session.clone(),
                id,
                Arc::new(sink.clone()),
            );
            (shared, sink)
        };
        let (first, first_sink) = make(1);
        let (second, _second_sink) = make(2);

        // The other trailer takes all but one byte of the pool, leaving this
        // one holding far less than its coalescing interval.
        assert_eq!(RecvShared::accept_bytes(&first, 1), None);
        assert_eq!(RecvShared::accept_bytes(&second, 63), None);

        let mut trailer = TrailerRecv::new(first.clone());
        trailer.set_manual_credit();
        trailer.release(1);
        assert_eq!(
            &*lock(&first_sink.credits),
            &[(1, 1)],
            "a drained pool must flush even a single byte, whoever drained it"
        );
    }

    /// The pool ledger must survive the objects it accounts for: settlement
    /// is idempotent and refunds are clamped, so an abort and a `Credit` that
    /// crossed it on the wire cannot both pay off the same bytes.
    #[test]
    fn session_ledger_settles_each_byte_exactly_once() {
        let session = SessionWindow::new(100);
        assert_eq!(session.debit_up_to(1, 40), 40);
        assert_eq!(session.debit_up_to(2, 30), 30);
        assert_eq!(session.available(), 30);

        // A refund larger than what the id owes is clamped, not overpaid.
        session.refund(1, 1000);
        assert_eq!(session.available(), 70);
        // ... and repeating it adds nothing.
        session.refund(1, 1000);
        assert_eq!(session.available(), 70);

        // Settling returns the remainder once, and only once.
        session.settle(2);
        assert_eq!(session.available(), 100);
        session.settle(2);
        session.refund(2, 30);
        assert_eq!(session.available(), 100);

        // An id the pool never knew about is a no-op, which is what makes a
        // credit for an already-finished trailer safe to apply blindly.
        session.refund(99, 10);
        session.settle(99);
        assert_eq!(session.available(), 100);
    }

    /// `discard` returns the trailer's whole outstanding debt to the pool in
    /// one go, so bytes still sitting in `stage` and read afterwards must not
    /// be refunded a second time.
    #[test]
    fn reading_staged_bytes_after_a_discard_does_not_double_refund() {
        let limits = Limits {
            trailer_credit_interval: 8,
            trailer_session_window: 64,
            ..Limits::default()
        };
        let session = Arc::new(SessionWindow::new(limits.trailer_session_window));
        let sink = Arc::new(RecordingSink::default());
        let shared = RecvShared::new(
            limits.trailer_recv_copy_threshold,
            limits.trailer_recv_demand_copy_threshold,
            limits.trailer_credit_interval,
            session.clone(),
            7,
            Arc::new(sink.clone()),
        );

        assert!(RecvShared::accept_bytes(&shared, 8).is_none());
        assert_eq!(session.available(), 56);
        lock(&shared).stage.extend_from_slice(b"abcdefgh");

        let mut trailer = TrailerRecv::new(shared.clone());
        RecvShared::discard(&shared);
        assert_eq!(session.available(), 64, "the debt is returned in full");

        // The staged tail is still readable, and must cost the pool nothing.
        let mut out = [0u8; 8];
        assert!(matches!(
            poll_read_once(&mut trailer, &mut out),
            Poll::Ready(Ok(8))
        ));
        assert_eq!(session.available(), 64);
        assert!(lock(&sink.credits).is_empty());
    }

    /// Dropping the handle tells the peer at once, rather than waiting for
    /// another unwanted fragment that a parked sender will never send.
    #[test]
    fn dropping_a_trailer_discards_eagerly_exactly_once() {
        let (shared, sink) = recv_shared(unbounded_limits());
        let trailer = TrailerRecv::new(shared.clone());
        RecvShared::discard(&shared);
        assert_eq!(&*lock(&sink.discards), &[7]);
        drop(trailer);
        assert_eq!(&*lock(&sink.discards), &[7], "idempotent");
    }
}
