use crate::global::PipeTag;

use std::{
    cell::RefCell,
    collections::VecDeque,
    future, io, mem,
    ops::{Deref, DerefMut},
    rc::Rc,
    task::{Poll, Waker},
};

use dolang_vfs::process::{PipeRecv, PipeSend};
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};

use dolang::runtime::{
    AllocExt, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    io::{ByteHandle, ByteIo, IntoByteIo, IoCaps, Shared},
    object::{Mut, Ref, Request, TypeBuilder},
    unpack,
    value::{Nil, TypeObject},
    with_instance, with_slots,
};

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    global::PipeGlobal,
    io_mode::{IoMode, encode_value, read_raw, read_value, write_pieces},
};

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

enum EndState<T> {
    Absent,
    Present(T),
    Taken,
}

impl<T> EndState<T> {
    fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    fn is_taken(&self) -> bool {
        matches!(self, Self::Taken)
    }

    fn take(&mut self) -> Option<T> {
        match mem::replace(self, Self::Taken) {
            Self::Present(value) => Some(value),
            other => {
                *self = other;
                None
            }
        }
    }

    fn set_present(&mut self, value: T) {
        *self = Self::Present(value);
    }

    fn set_absent(&mut self) {
        *self = Self::Absent;
    }
}

enum PipeState {
    /// Both sides in value mode
    Value,
    /// Recv side is in pipe mode; send side is in value mode
    RecvPipe,
    /// Send side is in pipe mode; recv side is in value mode
    SendPipe,
    /// Both sides in value mode; data may remain in pipe
    Draining,
    /// Both sides in pipe mode
    Direct,
}

// ---------------------------------------------------------------------------
// Shared inner state
// ---------------------------------------------------------------------------

struct PipeChannelShared {
    /// Whether `PipeReceiver`'s GC slot 0 is logically occupied.
    ///
    /// Just a flag: a value is written to the pipe exactly as its own bytes, so
    /// if the channel later degrades to a real pipe the buffered value is
    /// serialized the same way it would have been in the first place. Nothing
    /// about how it was `put` needs remembering.
    buffered: bool,
    state: PipeState,
    send_end: EndState<PipeSend>,
    recv_end: EndState<BufReader<PipeRecv>>,
    send_closed: bool,
    recv_closed: bool,
    send_wakers: VecDeque<Waker>,
    recv_wakers: VecDeque<Waker>,
    negotiate_wakers: VecDeque<Waker>,
    /// Kernel buffer size requested when this channel is negotiated into a
    /// real OS pipe, or `None` for the default. Fixed at pair-construction
    /// time so negotiation can't forget to apply it.
    buffer_size: Option<usize>,
}

impl PipeChannelShared {
    fn new(buffer_size: Option<usize>) -> Self {
        Self {
            buffered: false,
            state: PipeState::Value,
            send_end: EndState::Absent,
            recv_end: EndState::Absent,
            send_closed: false,
            recv_closed: false,
            send_wakers: VecDeque::new(),
            recv_wakers: VecDeque::new(),
            negotiate_wakers: VecDeque::new(),
            buffer_size,
        }
    }

    fn wake_receivers(&mut self) {
        for w in self.recv_wakers.drain(..) {
            w.wake();
        }
    }

    fn wake_senders(&mut self) {
        for w in self.send_wakers.drain(..) {
            w.wake();
        }
    }

    fn wake_negotiators(&mut self) {
        for w in self.negotiate_wakers.drain(..) {
            w.wake();
        }
    }

    fn recv_done(&mut self) {
        match mem::replace(&mut self.state, PipeState::Value) {
            PipeState::RecvPipe => {
                if self.send_closed {
                    // The send side is permanently closed, so nothing more can
                    // arrive and no later negotiation can want this end. Drop it
                    // instead of parking in `Draining`: the read end is a VFS
                    // endpoint, and holding it keeps the session that owns it
                    // alive until the channel itself is closed, which happens
                    // outside the scope that owns the session.
                    //
                    // This discards any bytes the program left unread in the
                    // pipe. Sequential *writers* upstream are well defined and
                    // preserved by `Draining`, but sequential readers downstream
                    // are already a crapshoot, in Do and in plain Unix alike:
                    //
                    //     cat file | { grep -m 1 foobar; cat }
                    //
                    // only forwards the remainder if `grep` never read past the
                    // matching line's newline, and nothing reads a byte at a
                    // time. So there is no expectation here to break.
                    self.recv_end.set_absent();
                    self.wake_senders();
                    self.wake_receivers();
                    self.wake_negotiators();
                } else if self.send_end.is_absent() {
                    self.state = PipeState::Draining;
                    self.wake_receivers();
                    self.wake_negotiators();
                } else {
                    self.recv_end.set_absent();
                    self.send_end.set_absent();
                    self.wake_senders();
                    self.wake_receivers();
                    self.wake_negotiators();
                }
            }
            PipeState::Direct => {
                self.state = PipeState::SendPipe;
                self.wake_receivers();
                self.wake_negotiators();
            }
            _ => unreachable!("should be in recv-pipe state"),
        }
    }

    fn send_done(&mut self) {
        match mem::replace(&mut self.state, PipeState::Value) {
            PipeState::SendPipe => {
                self.send_end.set_absent();
                self.state = PipeState::Draining;
                self.wake_receivers();
                self.wake_negotiators();
            }
            PipeState::Direct => {
                self.send_end.set_absent();
                self.state = PipeState::RecvPipe;
                self.wake_senders();
                self.wake_negotiators();
            }
            _ => unreachable!("should be in send-pipe state"),
        }
    }

    fn close_recv(&mut self) {
        self.recv_closed = true;
        self.recv_end.set_absent();
        self.wake_senders();
        self.wake_receivers();
        self.wake_negotiators();
    }

    fn close_send(&mut self) {
        self.send_closed = true;
        self.send_end.set_absent();
        self.wake_senders();
        self.wake_receivers();
        self.wake_negotiators();
    }

    fn restore_send_end(&mut self, sender: PipeSend) {
        if self.send_closed {
            drop(sender);
            self.wake_senders();
            self.wake_receivers();
            self.wake_negotiators();
            return;
        }

        match self.state {
            PipeState::RecvPipe | PipeState::SendPipe | PipeState::Direct => {
                self.send_end.set_present(sender);
                self.wake_senders();
                self.wake_negotiators();
            }
            PipeState::Value | PipeState::Draining => {
                drop(sender);
            }
        }
    }

    /// Takes back the read end a [`PipeStream`] held and leaves receive-pipe
    /// mode as a program's exit does.
    ///
    /// The stream's read-ahead is discarded with its buffer. With a Do sender
    /// the pipe itself goes too, so whatever part of a value the stream left
    /// unread is never seen: the next read gets the next value sent.
    fn stream_recv_done(&mut self, end: BufReader<PipeRecv>) {
        if !self.recv_closed {
            self.recv_end.set_present(BufReader::new(end.into_inner()));
        }
        self.recv_done();
    }

    /// Takes back the write end a [`PipeStream`] held and leaves send-pipe
    /// mode.
    ///
    /// Unlike a program stage's exit, which ends the stage's output, the
    /// sender stays usable: with a program reading, the end goes back to the
    /// channel so later values are written into the same pipe.
    fn stream_send_done(&mut self, end: PipeSend) {
        match mem::replace(&mut self.state, PipeState::Value) {
            PipeState::SendPipe => {
                drop(end);
                self.send_end.set_absent();
                self.state = PipeState::Draining;
            }
            PipeState::Direct => {
                if self.send_closed {
                    drop(end);
                    self.send_end.set_absent();
                } else {
                    self.send_end.set_present(end);
                }
                self.state = PipeState::RecvPipe;
            }
            _ => unreachable!("should be in send-pipe state"),
        }
        self.wake_senders();
        self.wake_receivers();
        self.wake_negotiators();
    }

    fn discard_recv_buffer(&mut self) {
        self.recv_end = match mem::replace(&mut self.recv_end, EndState::Taken) {
            EndState::Absent => EndState::Absent,
            EndState::Taken => EndState::Taken,
            EndState::Present(recv_end) => EndState::Present(BufReader::new(recv_end.into_inner())),
        };
    }
}

async fn drain_pipe(mut reader: BufReader<PipeRecv>, send_end: &mut PipeSend) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        send_end.write_all(&buf[..n]).await?;
    }
}

fn take_buffered_bytes<'v, 's>(
    recv_inst: Instance<'v, '_, PipeReceiver>,
    strand: &mut Strand<'v, 's>,
    inner: &mut PipeChannelShared,
) -> Result<'v, 's, Option<Vec<u8>>> {
    if !inner.buffered {
        return Ok(None);
    }

    let mut recv_borrow = recv_inst.borrow_mut(strand)?;
    let mut slot = Mut::slot_mut::<0>(&mut recv_borrow);
    let bytes = encode_value(strand, &Slot::reborrow(&mut slot))?;
    Output::set(strand, slot, Nil);
    inner.buffered = false;
    inner.wake_senders();
    inner.wake_negotiators();
    Ok(Some(bytes))
}

struct SendEndGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
    end: Option<PipeSend>,
}

impl SendEndGuard {
    fn new(shared: Rc<RefCell<PipeChannelShared>>, end: PipeSend) -> Self {
        Self {
            shared,
            end: Some(end),
        }
    }

    fn take(shared: &Rc<RefCell<PipeChannelShared>>) -> io::Result<Self> {
        let end = shared
            .borrow_mut()
            .send_end
            .take()
            .ok_or_else(|| io::Error::other("pipe end unavailable"))?;
        Ok(Self {
            shared: shared.clone(),
            end: Some(end),
        })
    }

    /// Takes the end out of the guard so that `Drop` does not restore it,
    /// leaving the channel's slot `Taken`.
    fn steal(mut self) -> PipeSend {
        self.end.take().expect("send end taken twice")
    }
}

impl AsRef<PipeSend> for SendEndGuard {
    fn as_ref(&self) -> &PipeSend {
        self.end.as_ref().unwrap()
    }
}

impl AsMut<PipeSend> for SendEndGuard {
    fn as_mut(&mut self) -> &mut PipeSend {
        self.end.as_mut().unwrap()
    }
}

impl Deref for SendEndGuard {
    type Target = PipeSend;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl DerefMut for SendEndGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut()
    }
}

impl Drop for SendEndGuard {
    fn drop(&mut self) {
        if let Some(end) = self.end.take() {
            self.shared.borrow_mut().restore_send_end(end);
        }
    }
}

struct RecvEndGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
    end: Option<BufReader<PipeRecv>>,
}

impl RecvEndGuard {
    fn take(shared: &Rc<RefCell<PipeChannelShared>>) -> io::Result<Self> {
        let end = shared
            .borrow_mut()
            .recv_end
            .take()
            .ok_or_else(|| io::Error::other("pipe end unavailable"))?;
        Ok(Self {
            shared: shared.clone(),
            end: Some(end),
        })
    }

    fn discard(mut self) {
        let _ = self.end.take();
    }

    /// Takes the end out of the guard so that `Drop` does not restore it,
    /// leaving the channel's slot `Taken`.
    fn steal(mut self) -> BufReader<PipeRecv> {
        self.end.take().expect("recv end taken twice")
    }
}

impl AsRef<BufReader<PipeRecv>> for RecvEndGuard {
    fn as_ref(&self) -> &BufReader<PipeRecv> {
        self.end.as_ref().unwrap()
    }
}

impl AsMut<BufReader<PipeRecv>> for RecvEndGuard {
    fn as_mut(&mut self) -> &mut BufReader<PipeRecv> {
        self.end.as_mut().unwrap()
    }
}

impl Deref for RecvEndGuard {
    type Target = BufReader<PipeRecv>;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl DerefMut for RecvEndGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut()
    }
}

impl Drop for RecvEndGuard {
    fn drop(&mut self) {
        if let Some(end) = self.end.take() {
            let mut inner = self.shared.borrow_mut();
            if inner.recv_closed {
                drop(end);
                inner.wake_senders();
                inner.wake_receivers();
                inner.wake_negotiators();
            } else {
                inner.recv_end.set_present(end);
                inner.wake_receivers();
                inner.wake_negotiators();
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct PipeAnnex<'v> {
    shared: Rc<RefCell<PipeChannelShared>>,
    global: State<'v, PipeGlobal<'v>>,
}

// ---------------------------------------------------------------------------
// RAII guards — dropped when the corresponding program exits
// ---------------------------------------------------------------------------

pub(crate) struct RecvGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
    armed: bool,
}

impl RecvGuard {
    /// Takes the read end for a [`PipeStream`], which releases it itself
    /// rather than when this guard drops.
    fn into_stream_end(mut self) -> io::Result<BufReader<PipeRecv>> {
        let end = RecvEndGuard::take(&self.shared)?.steal();
        self.armed = false;
        Ok(end)
    }

    pub(crate) async fn recv_pipe(&self) -> io::Result<PipeRecv> {
        let reader = RecvEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: consumer end closed"))?;
        Ok(reader.get_ref().try_clone().await?)
    }

    /// Hands the read end to the caller outright, leaving the channel without
    /// a descriptor of its own.
    ///
    /// See [`SendGuard::steal_send_pipe`] for why a long-lived consumer takes
    /// its ends rather than duplicating them.
    ///
    /// Any bytes sitting in the reader's buffer are dropped along with it,
    /// exactly as [`Self::recv_pipe`] drops them by duplicating the descriptor
    /// out from under the same buffer. Negotiation hands buffered bytes back
    /// to the channel before either is reached, so there are none.
    pub(crate) fn steal_recv_pipe(&self) -> io::Result<PipeRecv> {
        let reader = RecvEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: consumer end closed"))?;
        debug_assert!(
            reader.buffer().is_empty(),
            "stealing a read end would discard buffered bytes"
        );
        Ok(reader.steal().into_inner())
    }
}

impl Drop for RecvGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.borrow_mut().recv_done();
        }
    }
}

pub(crate) struct SendGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
    armed: bool,
}

impl SendGuard {
    /// Takes the write end for a [`PipeStream`], which releases it itself
    /// rather than when this guard drops.
    fn into_stream_end(mut self) -> io::Result<PipeSend> {
        let end = SendEndGuard::take(&self.shared)?.steal();
        self.armed = false;
        Ok(end)
    }

    pub(crate) async fn send_pipe(&self) -> io::Result<PipeSend> {
        let sender = SendEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: producer end closed"))?;
        Ok(sender.try_clone().await?)
    }

    /// Hands the write end to the caller outright, leaving the channel without
    /// a descriptor of its own.
    ///
    /// Duplicating, as [`Self::send_pipe`] does, is right for a pipeline
    /// stage: the channel keeps a descriptor so the stage can renegotiate
    /// between value and byte framing, and the duplicate the stage holds is
    /// released when the stage exits — which happens once the external program
    /// it feeds has exited, so the pipeline never stalls on it.
    ///
    /// A remote `shell.Vfs` session is not a pipeline stage. It outlives every
    /// such exit, so a descriptor left behind in the channel is one that
    /// nothing closes for as long as the session runs. On the write side that
    /// holds the pipe open after the peer is gone, so the reader on the far
    /// end never sees EOF and a disconnected server is indistinguishable from
    /// a hung one. Taking the ends outright is what makes the disconnect
    /// observable.
    pub(crate) fn steal_send_pipe(&self) -> io::Result<PipeSend> {
        let sender = SendEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: producer end closed"))?;
        Ok(sender.steal())
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.borrow_mut().send_done();
        }
    }
}

// ---------------------------------------------------------------------------
// GC object types
// ---------------------------------------------------------------------------

/// The reading end of a pipe channel.
///
/// Bytes from a program are read as lines. A Do stage wanting them unframed
/// asks for a [`PipeStream`] with `io()`.
pub(crate) struct PipeReceiver;

pub(crate) struct PipeSender;

/// The pipe end a [`PipeStream`] holds until it is closed.
enum StreamEnd {
    Recv(Shared<BufReader<PipeRecv>>),
    Send(Shared<PipeSend>),
}

/// The byte stream a channel end's `io()` returns: an `io.Io` over the OS pipe
/// negotiated for it.
///
/// It takes the end from the channel, whose value operations on that end
/// raise `ConcurrencyError` until `close` gives it back. Handles lent through
/// [`ByteIo`] share the end and fail once it is closed.
pub(crate) struct PipeStream {
    end: Option<StreamEnd>,
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<BufReader<PipeRecv>>();
    assert_send::<PipeSend>();
};

pub(crate) struct PipeStreamAnnex {
    shared: Rc<RefCell<PipeChannelShared>>,
}

impl<'v> IntoByteIo<'v> for PipeStream {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(match self.end {
            Some(StreamEnd::Recv(_)) => IoCaps::READ,
            Some(StreamEnd::Send(_)) => IoCaps::WRITE,
            None => IoCaps::empty(),
        })
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        Ok(match self.end.as_ref().expect("stream is closed") {
            StreamEnd::Recv(reader) => ByteHandle::read(reader.clone()),
            StreamEnd::Send(writer) => ByteHandle::write(writer.clone()),
        })
    }
}

/// Creates the stream for a channel end into `out`, or, with a block, runs the
/// block with it and closes it afterward.
async fn open_stream<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, PipeGlobal<'v>>,
    shared: Rc<RefCell<PipeChannelShared>>,
    end: StreamEnd,
    block: Option<&Value<'v>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let stream = PipeStream { end: Some(end) };
    let annex = PipeStreamAnnex { shared };
    let Some(block) = block else {
        global
            .types
            .pipe_stream
            .create_with_annex(strand, stream, annex, out);
        return Ok(());
    };
    with_slots!(strand, |mut handle| {
        global.types.pipe_stream.create_with_annex(
            strand,
            stream,
            annex,
            Slot::reborrow(&mut handle),
        );
        let result = call!(strand, block, out, &handle).await;
        // Always released, even when the block raised.
        let closed = with_instance!(
            strand,
            global.types.pipe_stream.cast(&handle).unwrap(),
            |inst| { close_stream(inst, strand).await }
        );
        result.and(closed)
    })
}

/// Gives the stream's end back to its channel. Closing again does nothing.
async fn close_stream<'v, 's>(
    inst: Instance<'v, '_, PipeStream>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, ()> {
    let end = inst.borrow_mut(strand)?.end.take();
    let shared = &inst.annex().shared;
    match end {
        None => {}
        Some(StreamEnd::Recv(mut reader)) => {
            // Waits out an operation on a lent handle, then ends them all.
            if let Some(reader) = reader.close().await {
                shared.borrow_mut().stream_recv_done(reader);
            }
        }
        Some(StreamEnd::Send(mut writer)) => {
            if let Some(mut writer) = writer.close().await {
                let flushed = writer.flush().await;
                shared.borrow_mut().stream_send_done(writer);
                flushed.into_sys(strand)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Async negotiate functions
// ---------------------------------------------------------------------------

/// Whether `value` is a pipe channel's receiving end, which negotiates a pipe
/// instead of being read as a stream.
pub(crate) fn is_receiver<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    strand
        .try_state::<PipeTag>()
        .is_some_and(|global| global.types.pipe_receiver.cast(value).is_some())
}

/// Whether `value` is a pipe channel's sending end, which negotiates a pipe
/// instead of being written as a stream.
pub(crate) fn is_sender<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    strand
        .try_state::<PipeTag>()
        .is_some_and(|global| global.types.pipe_sender.cast(value).is_some())
}

pub(crate) async fn negotiate_recv<'v, 's>(
    input: &Value<'v>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Option<RecvGuard>> {
    let Some(global) = strand.try_state::<PipeTag>() else {
        return Ok(None);
    };
    let Some(cast) = global.types.pipe_receiver.cast(input) else {
        return Ok(None);
    };
    with_instance!(strand, cast, |inst| {
        lease_recv(inst, strand, false).await.map(Some)
    })
}

async fn lease_recv<'v, 's>(
    inst: Instance<'v, '_, PipeReceiver>,
    strand: &mut Strand<'v, 's>,
    keep_buffer: bool,
) -> Result<'v, 's, RecvGuard> {
    let global = inst.annex().global;
    let shared = &inst.annex().shared;
    let mut fresh_pipe = None;
    let (send_end, old_recv_end, stale_bytes) = loop {
        let ready = future::poll_fn(|cx| {
            let mut inner = shared.borrow_mut();
            match inner.state {
                PipeState::Value => Poll::Ready(true),
                PipeState::SendPipe | PipeState::Draining if !inner.recv_end.is_taken() => {
                    Poll::Ready(true)
                }
                PipeState::RecvPipe => Poll::Ready(false),
                _ => {
                    inner.negotiate_wakers.push_back(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;

        if !ready {
            return Err(Error::concurrency_msg(strand, "program owns channel end"));
        }

        let needs_pipe = {
            let inner = shared.borrow();
            matches!(inner.state, PipeState::Value)
                || matches!(inner.state, PipeState::Draining) && !inner.send_closed
        };
        if needs_pipe && fresh_pipe.is_none() {
            let buffer_size = shared.borrow().buffer_size;
            let local = global.local.get(strand);
            fresh_pipe = Some(local.vfs().pipe(buffer_size).await.into_sys(strand)?);
            continue;
        }

        let result = {
            let mut inner = shared.borrow_mut();
            match &mut inner.state {
                PipeState::Value => {
                    let stale_bytes = take_buffered_bytes(inst, strand, &mut inner)?;
                    let (w, r) = fresh_pipe.take().unwrap();
                    inner.recv_end.set_present(BufReader::new(r));
                    inner.send_end = EndState::Taken;
                    inner.state = PipeState::RecvPipe;
                    inner.wake_senders();
                    (Some(w), None, stale_bytes)
                }
                PipeState::SendPipe => {
                    let send_end = mem::replace(&mut inner.send_end, EndState::Taken);
                    // A program gets a duplicate of the descriptor and never
                    // sees this buffer. A stream reads through it, so bytes
                    // already read ahead stay readable.
                    if !keep_buffer {
                        inner.discard_recv_buffer();
                    }
                    inner.state = PipeState::Direct;
                    inner.send_end = send_end;
                    (None, None, None)
                }
                PipeState::Draining => {
                    if inner.send_closed {
                        inner.state = PipeState::RecvPipe;
                        inner.wake_senders();
                        (None, None, None)
                    } else {
                        let old_recv_end = inner.recv_end.take().unwrap();
                        let (w, r) = fresh_pipe.take().unwrap();
                        inner.recv_end.set_present(BufReader::new(r));
                        inner.send_end = EndState::Taken;
                        inner.state = PipeState::RecvPipe;
                        inner.wake_senders();
                        (Some(w), Some(old_recv_end), None)
                    }
                }
                _ => unreachable!("state changed after readiness was revalidated"),
            }
        };
        break result;
    };

    let mut send_end = send_end.map(|send_end| SendEndGuard::new(shared.clone(), send_end));

    if let Some(mut send_end) = send_end.take() {
        if let Some(old_recv_end) = old_recv_end {
            drain_pipe(old_recv_end, &mut send_end)
                .await
                .into_sys(strand)?;
        }
        if let Some(stale_bytes) = stale_bytes {
            strand.spawn_task(async move {
                let _ = send_end.write_all(&stale_bytes).await;
            });
        }
    }

    Ok(RecvGuard {
        shared: shared.clone(),
        armed: true,
    })
}

pub(crate) async fn negotiate_send<'v, 's>(
    output: &Value<'v>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Option<SendGuard>> {
    let Some(global) = strand.try_state::<PipeTag>() else {
        return Ok(None);
    };
    let Some(cast) = global.types.pipe_sender.cast(output) else {
        return Ok(None);
    };
    with_instance!(strand, cast, |inst| {
        lease_send(inst, strand).await.map(Some)
    })
}

async fn lease_send<'v, 's>(
    inst: Instance<'v, '_, PipeSender>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, SendGuard> {
    let global = inst.annex().global;
    let shared = &inst.annex().shared;
    let mut fresh_pipe = None;
    let (send_end, old_recv_end, stale_bytes) = loop {
        let ready = future::poll_fn(|cx| {
            let mut inner = shared.borrow_mut();
            match inner.state {
                PipeState::Value => Poll::Ready(true),
                PipeState::RecvPipe if !inner.send_end.is_taken() => Poll::Ready(true),
                PipeState::Draining if !inner.recv_end.is_taken() => Poll::Ready(true),
                PipeState::SendPipe | PipeState::Direct => Poll::Ready(false),
                _ => {
                    inner.negotiate_wakers.push_back(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;

        if !ready {
            return Err(Error::concurrency_msg(strand, "program owns channel end"));
        }

        let needs_pipe = matches!(
            shared.borrow().state,
            PipeState::Value | PipeState::Draining
        );
        if needs_pipe && fresh_pipe.is_none() {
            let buffer_size = shared.borrow().buffer_size;
            let local = global.local.get(strand);
            fresh_pipe = Some(local.vfs().pipe(buffer_size).await.into_sys(strand)?);
            continue;
        }

        let result = {
            let mut inner = shared.borrow_mut();
            match &mut inner.state {
                PipeState::Value => {
                    let send_borrow = inst.borrow(strand)?;
                    let stale_bytes = global
                        .types
                        .pipe_receiver
                        .cast(Ref::slot::<0>(&send_borrow))
                        .unwrap()
                        .enter_sync(strand, |strand, recv_inst| {
                            take_buffered_bytes(recv_inst, strand, &mut inner)
                        })?;
                    drop(send_borrow);
                    let (w, r) = fresh_pipe.take().unwrap();
                    inner.send_end = EndState::Taken;
                    inner.recv_end.set_present(BufReader::new(r));
                    inner.state = PipeState::SendPipe;
                    inner.wake_receivers();
                    (Some(w), None, stale_bytes)
                }
                PipeState::RecvPipe => {
                    let send_end = inner.send_end.take().unwrap();
                    inner.send_end = EndState::Taken;
                    inner.state = PipeState::Direct;
                    (Some(send_end), None, None)
                }
                PipeState::Draining => {
                    let old_recv_end = inner.recv_end.take().unwrap();
                    let (w, r) = fresh_pipe.take().unwrap();
                    inner.send_end = EndState::Taken;
                    inner.recv_end.set_present(BufReader::new(r));
                    inner.state = PipeState::SendPipe;
                    inner.wake_receivers();
                    (Some(w), Some(old_recv_end), None)
                }
                _ => unreachable!("state changed after readiness was revalidated"),
            }
        };
        break result;
    };

    let mut send_end = send_end.map(|send_end| SendEndGuard::new(shared.clone(), send_end));

    if let Some(mut send_end) = send_end.take() {
        if let Some(old_recv_end) = old_recv_end {
            drain_pipe(old_recv_end, &mut send_end)
                .await
                .into_sys(strand)?;
        }
        if let Some(stale_bytes) = stale_bytes {
            send_end.write_all(&stale_bytes).await.into_sys(strand)?;
        }
    }

    Ok(SendGuard {
        shared: shared.clone(),
        armed: true,
    })
}

pub(crate) fn install<'v>(builder: &mut dolang::runtime::vm::Builder<'v>) {
    builder.pipe_handler(move |strand, out_send, out_recv| {
        make_pair(strand, out_send, out_recv, None);
    });
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

pub(crate) fn make_pair<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut out_send: Slot<'v, '_>,
    mut out_recv: Slot<'v, '_>,
    buffer_size: Option<usize>,
) {
    let inner = Rc::new(RefCell::new(PipeChannelShared::new(buffer_size)));
    let global = strand.force_state::<PipeTag>();
    let recv_annex = PipeAnnex {
        shared: inner.clone(),
        global,
    };
    let send_annex = PipeAnnex {
        shared: inner,
        global,
    };

    global.types.pipe_receiver.create_with_annex(
        strand,
        PipeReceiver,
        recv_annex,
        Slot::reborrow(&mut out_recv),
    );
    global.types.pipe_sender.create_with_annex(
        strand,
        PipeSender,
        send_annex,
        Slot::reborrow(&mut out_send),
    );

    global
        .types
        .pipe_sender
        .cast(&out_send)
        .unwrap()
        .enter_sync(strand, |strand, send_inst| {
            let mut send_borrow = send_inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut send_borrow), &out_recv);
        });
}

impl<'v> Object<'v> for PipeReceiver {
    const MODULE: &'v str = "proc";
    const NAME: &'v str = "PipeReceiver";
    const SLOTS: usize = 3;
    type Annex = PipeAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.annex().shared.borrow_mut().close_recv();
                Ok(())
            })
            .method("io", async move |this, strand, args, out| {
                let ([], [block]) = unpack!(strand, args, 0, 1)?;
                let guard = lease_recv(this, strand, true).await?;
                let reader = guard.into_stream_end().into_sys(strand)?;
                let annex = this.annex();
                open_stream(
                    strand,
                    annex.global,
                    annex.shared.clone(),
                    StreamEnd::Recv(Shared::new(reader)),
                    block.as_deref(),
                    out,
                )
                .await
            })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let shared = &this.annex().shared;

        loop {
            let recv_end = {
                let mut inner = shared.borrow_mut();
                if inner.recv_closed {
                    return Err(Error::state_error(strand, "channel end is closed"));
                }
                let send_closed = inner.send_closed;
                match &mut inner.state {
                    PipeState::Value => {
                        if inner.buffered {
                            inner.buffered = false;
                            drop(inner);
                            let mut borrow = this.borrow_mut(strand)?;
                            Output::set(strand, out, Mut::slot_mut::<0>(&mut borrow));
                            drop(borrow);
                            let mut inner = shared.borrow_mut();
                            inner.wake_senders();
                            inner.wake_negotiators();
                            return Ok(true);
                        }
                        if send_closed {
                            let borrow = this.borrow(strand)?;
                            if Ref::slot::<1>(&borrow).is_nil() {
                                return Ok(false);
                            }
                            let err = if Ref::slot::<2>(&borrow).is_nil() {
                                Error::from_value(strand, Ref::slot::<1>(&borrow))
                            } else {
                                Error::from_value_backtrace(
                                    strand,
                                    Ref::slot::<1>(&borrow),
                                    Ref::slot::<2>(&borrow),
                                )
                                .expect("invalid backtrace")
                            };
                            drop(borrow);
                            return Err(err);
                        }
                        None
                    }
                    PipeState::SendPipe => {
                        if inner.recv_end.is_present() {
                            drop(inner);
                            Some(RecvEndGuard::take(shared).into_sys(strand)?)
                        } else {
                            return Err(Error::concurrency_msg(strand, "program owns channel end"));
                        }
                    }
                    PipeState::Draining => {
                        if inner.recv_end.is_present() {
                            drop(inner);
                            Some(RecvEndGuard::take(shared).into_sys(strand)?)
                        } else if inner.recv_end.is_absent() {
                            let mut inner = shared.borrow_mut();
                            inner.state = PipeState::Value;
                            inner.wake_senders();
                            inner.wake_receivers();
                            inner.wake_negotiators();
                            None
                        } else {
                            return Err(Error::concurrency_msg(strand, "program owns channel end"));
                        }
                    }
                    PipeState::RecvPipe | PipeState::Direct => {
                        return Err(Error::concurrency_msg(strand, "program owns channel end"));
                    }
                }
            };

            if let Some(mut reader) = recv_end {
                match read_value(&mut *reader, IoMode::Line, strand, &mut out).await {
                    Ok(false) => {
                        reader.discard();
                        let mut inner = shared.borrow_mut();
                        inner.state = PipeState::Value;
                        inner.wake_senders();
                        inner.wake_receivers();
                        inner.wake_negotiators();
                        continue;
                    }
                    Ok(true) => return Ok(true),
                    Err(e) => {
                        return Err(e.into_sys(strand));
                    }
                }
            }

            future::poll_fn(|cx| {
                let mut inner = shared.borrow_mut();
                let ready = match &inner.state {
                    PipeState::Value => inner.buffered || inner.send_closed || inner.recv_closed,
                    PipeState::SendPipe | PipeState::Draining => {
                        inner.recv_end.is_present() || inner.send_closed || inner.recv_closed
                    }
                    PipeState::RecvPipe | PipeState::Direct => true,
                };
                if ready {
                    Poll::Ready(())
                } else {
                    inner.recv_wakers.push_back(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
        }
    }
}

impl<'v> Object<'v> for PipeSender {
    const MODULE: &'v str = "proc";
    const NAME: &'v str = "PipeSender";
    const SLOTS: usize = 1;
    type Annex = PipeAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let backtrace_sym = builder.sym("backtrace");
        builder
            .supertype(TypeObject::Sink)
            .method("io", async move |this, strand, args, out| {
                let ([], [block]) = unpack!(strand, args, 0, 1)?;
                let guard = lease_send(this, strand).await?;
                let writer = guard.into_stream_end().into_sys(strand)?;
                let annex = this.annex();
                open_stream(
                    strand,
                    annex.global,
                    annex.shared.clone(),
                    StreamEnd::Send(Shared::new(writer)),
                    block.as_deref(),
                    out,
                )
                .await
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], [err, backtrace]) = unpack!(strand, args, 0, 1, backtrace_sym = None)?;
                let shared = &this.annex().shared;
                let send_borrow = this.borrow(strand)?;
                this.annex()
                    .global
                    .types
                    .pipe_receiver
                    .cast(Ref::slot::<0>(&send_borrow))
                    .unwrap()
                    .enter_sync(strand, |strand, recv_inst| {
                        let mut recv_borrow = recv_inst.borrow_mut(strand)?;
                        if let Some(err) = err {
                            if let Some(backtrace) = backtrace {
                                if backtrace.as_backtrace(strand.vm()).is_none() {
                                    return Err(Error::type_error(
                                        strand,
                                        "expected inspect.Backtrace",
                                    ));
                                }
                                Output::set(
                                    strand,
                                    Mut::slot_mut::<2>(&mut recv_borrow),
                                    backtrace,
                                );
                            } else {
                                Output::set(strand, Mut::slot_mut::<2>(&mut recv_borrow), Nil);
                            }
                            Output::set(strand, Mut::slot_mut::<1>(&mut recv_borrow), err);
                        }
                        Ok(())
                    })?;
                shared.borrow_mut().close_send();
                Ok(())
            })
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let shared = &this.annex().shared;
        let global = this.annex().global;

        loop {
            let send_end = {
                let mut inner = shared.borrow_mut();
                if inner.send_closed {
                    return Err(Error::state_error(strand, "channel end is closed"));
                }
                if inner.recv_closed {
                    return Err(Error::sink_stop(strand));
                }
                match &mut inner.state {
                    PipeState::Value => {
                        if inner.buffered {
                            None
                        } else {
                            inner.buffered = true;
                            drop(inner);
                            let send_borrow = this.borrow(strand)?;
                            global
                                .types
                                .pipe_receiver
                                .cast(Ref::slot::<0>(&send_borrow))
                                .unwrap()
                                .enter_sync(strand, |strand, recv_inst| {
                                    let mut recv_borrow = recv_inst.borrow_mut(strand)?;
                                    Output::set(
                                        strand,
                                        Mut::slot_mut::<0>(&mut recv_borrow),
                                        value,
                                    );
                                    Ok(())
                                })?;
                            drop(send_borrow);
                            shared.borrow_mut().wake_receivers();
                            return Ok(());
                        }
                    }
                    PipeState::RecvPipe => {
                        if inner.send_end.is_present() {
                            drop(inner);
                            Some(SendEndGuard::take(shared).into_sys(strand)?)
                        } else if inner.send_end.is_taken() {
                            // Another write from Do holds the end, such as the
                            // value a receiving stream took over when it was
                            // opened. Wait for it to finish.
                            None
                        } else {
                            return Err(Error::concurrency_msg(strand, "program owns channel end"));
                        }
                    }
                    PipeState::Draining => None,
                    PipeState::SendPipe | PipeState::Direct => {
                        return Err(Error::concurrency_msg(strand, "program owns channel end"));
                    }
                }
            };

            if let Some(mut writer) = send_end {
                let bytes = encode_value(strand, &value)?;
                return match writer.write_all(&bytes).await {
                    Ok(()) => Ok(()),
                    // A receiving stream released the pipe mid-write, which
                    // consumes the value as a program exiting would.
                    Err(e)
                        if e.kind() == io::ErrorKind::BrokenPipe
                            && matches!(shared.borrow().state, PipeState::Value) =>
                    {
                        Ok(())
                    }
                    Err(e) => Err(e.into_sys(strand)),
                };
            }

            future::poll_fn(|cx| {
                let mut inner = shared.borrow_mut();
                let ready = match &inner.state {
                    PipeState::Value => !inner.buffered || inner.send_closed || inner.recv_closed,
                    PipeState::RecvPipe => {
                        inner.send_end.is_present() || inner.send_closed || inner.recv_closed
                    }
                    PipeState::Draining => inner.send_closed || inner.recv_closed,
                    PipeState::SendPipe | PipeState::Direct => true,
                };
                if ready {
                    Poll::Ready(())
                } else {
                    inner.send_wakers.push_back(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
        }
    }
}

impl PipeStream {
    fn reader(&self) -> Option<Shared<BufReader<PipeRecv>>> {
        match &self.end {
            Some(StreamEnd::Recv(reader)) => Some(reader.clone()),
            _ => None,
        }
    }

    fn writer(&self) -> Option<Shared<PipeSend>> {
        match &self.end {
            Some(StreamEnd::Send(writer)) => Some(writer.clone()),
            _ => None,
        }
    }
}

/// The error for `op` on a stream without the end it needs: closed, or open
/// in the other direction.
fn missing_end<'v, 's>(
    this: Instance<'v, '_, PipeStream>,
    strand: &mut Strand<'v, 's>,
    op: &'static str,
) -> Error<'v, 's> {
    match this.borrow(strand) {
        Ok(borrow) if borrow.end.is_none() => {
            drop(borrow);
            Error::state_error(strand, "stream closed")
        }
        Ok(borrow) => {
            drop(borrow);
            Error::unsupported(strand, op)
        }
        Err(e) => e,
    }
}

impl<'v> Object<'v> for PipeStream {
    const MODULE: &'v str = "proc";
    const NAME: &'v str = "PipeStream";
    type Annex = PipeStreamAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            if borrow.end.is_none() {
                return Err(Error::state_error(request, "stream closed"));
            }
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let readable = matches!(this.borrow(strand)?.end, Some(StreamEnd::Recv(_)));
                Output::set(strand, out, readable);
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let writable = matches!(this.borrow(strand)?.end, Some(StreamEnd::Send(_)));
                Output::set(strand, out, writable);
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size
                    .to_i64(strand)
                    .ok()
                    .and_then(|size| usize::try_from(size).ok())
                    .ok_or_else(|| {
                        Error::type_error(strand, "size must be a non-negative integer")
                    })?;
                let Some(mut reader) = this.borrow(strand)?.reader() else {
                    return Err(missing_end(this, strand, "`read`"));
                };
                let mut reader = reader.lock().await.into_sys(strand)?;
                read_raw(&mut *reader, Some(size), strand, out).await
            })
            .method("write", async move |this, strand, args, out| {
                let Some(writer) = this.borrow(strand)?.writer() else {
                    return Err(missing_end(this, strand, "`write`"));
                };
                write_pieces(&writer, args, strand, out).await
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let Some(mut writer) = this.borrow(strand)?.writer() else {
                    return Err(missing_end(this, strand, "`flush`"));
                };
                let mut writer = writer.lock().await.into_sys(strand)?;
                writer.flush().await.into_sys(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                close_stream(this, strand).await
            })
    }
}
