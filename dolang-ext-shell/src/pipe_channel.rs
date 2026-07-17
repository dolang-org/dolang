use std::{
    cell::RefCell,
    collections::VecDeque,
    future, io, mem,
    ops::{Deref, DerefMut},
    rc::Rc,
    task::{Poll, Waker},
};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use dolang::runtime::{
    Error, Format, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::{Nil, TypeObject, View},
};
use dolang_shell_vfs::{AnyVfs, Vfs};

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    global::Global,
    local::ChannelMode,
};

type StdioSend = <AnyVfs as Vfs>::StdioSend;
type StdioRecv = <AnyVfs as Vfs>::StdioRecv;

struct BytesFormat(Vec<u8>);

impl<'v> Format<'v> for BytesFormat {
    fn write_str<'s>(&mut self, _strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()> {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

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

enum BufferedValue {
    Empty,
    Line,
    Chunk,
}

// ---------------------------------------------------------------------------
// Shared inner state
// ---------------------------------------------------------------------------

struct PipeChannelShared {
    /// Whether PipeReceiver's GC slot 0 is logically occupied, and if so how it was written.
    buffered: BufferedValue,
    state: PipeState,
    send_end: EndState<StdioSend>,
    recv_end: EndState<BufReader<StdioRecv>>,
    send_closed: bool,
    recv_closed: bool,
    send_wakers: VecDeque<Waker>,
    recv_wakers: VecDeque<Waker>,
    negotiate_wakers: VecDeque<Waker>,
}

impl PipeChannelShared {
    fn new() -> Self {
        Self {
            buffered: BufferedValue::Empty,
            state: PipeState::Value,
            send_end: EndState::Absent,
            recv_end: EndState::Absent,
            send_closed: false,
            recv_closed: false,
            send_wakers: VecDeque::new(),
            recv_wakers: VecDeque::new(),
            negotiate_wakers: VecDeque::new(),
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
                if self.send_end.is_absent() {
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

    fn restore_send_end(&mut self, sender: StdioSend) {
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

    fn discard_recv_buffer(&mut self) {
        self.recv_end = match mem::replace(&mut self.recv_end, EndState::Taken) {
            EndState::Absent => EndState::Absent,
            EndState::Taken => EndState::Taken,
            EndState::Present(recv_end) => EndState::Present(BufReader::new(recv_end.into_inner())),
        };
    }
}

async fn drain_pipe(mut reader: BufReader<StdioRecv>, send_end: &mut StdioSend) -> io::Result<()> {
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
    let channel_mode = match inner.buffered {
        BufferedValue::Empty => return Ok(None),
        BufferedValue::Line => ChannelMode::Line,
        BufferedValue::Chunk => ChannelMode::Chunk,
    };

    let mut recv_borrow = recv_inst.borrow_mut(strand)?;
    let mut slot = Mut::slot_mut::<0>(&mut recv_borrow);
    let bytes = encode_value(strand, Slot::reborrow(&mut slot), channel_mode)?;
    Output::set(strand, slot, Nil);
    inner.buffered = BufferedValue::Empty;
    inner.wake_senders();
    inner.wake_negotiators();
    Ok(Some(bytes))
}

struct SendEndGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
    end: Option<StdioSend>,
}

impl SendEndGuard {
    fn new(shared: Rc<RefCell<PipeChannelShared>>, end: StdioSend) -> Self {
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
}

impl AsRef<StdioSend> for SendEndGuard {
    fn as_ref(&self) -> &StdioSend {
        self.end.as_ref().unwrap()
    }
}

impl AsMut<StdioSend> for SendEndGuard {
    fn as_mut(&mut self) -> &mut StdioSend {
        self.end.as_mut().unwrap()
    }
}

impl Deref for SendEndGuard {
    type Target = StdioSend;

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
    end: Option<BufReader<StdioRecv>>,
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
}

impl AsRef<BufReader<StdioRecv>> for RecvEndGuard {
    fn as_ref(&self) -> &BufReader<StdioRecv> {
        self.end.as_ref().unwrap()
    }
}

impl AsMut<BufReader<StdioRecv>> for RecvEndGuard {
    fn as_mut(&mut self) -> &mut BufReader<StdioRecv> {
        self.end.as_mut().unwrap()
    }
}

impl Deref for RecvEndGuard {
    type Target = BufReader<StdioRecv>;

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
    global: State<'v, Global<'v>>,
}

// ---------------------------------------------------------------------------
// RAII guards — dropped when the corresponding program exits
// ---------------------------------------------------------------------------

pub(crate) struct RecvGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
}

impl RecvGuard {
    pub(crate) async fn recv_pipe(&self) -> io::Result<StdioRecv> {
        let reader = RecvEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: consumer end closed"))?;
        reader.get_ref().try_clone().await
    }
}

impl Drop for RecvGuard {
    fn drop(&mut self) {
        self.shared.borrow_mut().recv_done();
    }
}

pub(crate) struct SendGuard {
    shared: Rc<RefCell<PipeChannelShared>>,
}

impl SendGuard {
    pub(crate) async fn send_pipe(&self) -> io::Result<StdioSend> {
        let sender = SendEndGuard::take(&self.shared)
            .map_err(|_| io::Error::other("pipe: producer end closed"))?;
        sender.try_clone().await
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        self.shared.borrow_mut().send_done();
    }
}

// ---------------------------------------------------------------------------
// GC object types
// ---------------------------------------------------------------------------

pub(crate) struct PipeReceiver;

pub(crate) struct PipeSender;

// ---------------------------------------------------------------------------
// Async negotiate functions
// ---------------------------------------------------------------------------

pub(crate) async fn negotiate_recv<'v, 's>(
    input: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, Option<RecvGuard>> {
    let Some(inst) = global.types.pipe_receiver.downcast(input) else {
        return Ok(None);
    };
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
            fresh_pipe = Some(
                global
                    .local
                    .get(strand)
                    .vfs()
                    .pipe()
                    .await
                    .into_sys(strand)?,
            );
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
                    inner.discard_recv_buffer();
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

    Ok(Some(RecvGuard {
        shared: shared.clone(),
    }))
}

pub(crate) async fn negotiate_send<'v, 's>(
    output: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, Option<SendGuard>> {
    let Some(inst) = global.types.pipe_sender.downcast(output) else {
        return Ok(None);
    };
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
            fresh_pipe = Some(
                global
                    .local
                    .get(strand)
                    .vfs()
                    .pipe()
                    .await
                    .into_sys(strand)?,
            );
            continue;
        }

        let result = {
            let mut inner = shared.borrow_mut();
            match &mut inner.state {
                PipeState::Value => {
                    let send_borrow = inst.borrow(strand)?;
                    let recv_inst = global
                        .types
                        .pipe_receiver
                        .downcast(Ref::slot::<0>(&send_borrow))
                        .unwrap();
                    let stale_bytes = take_buffered_bytes(recv_inst, strand, &mut inner)?;
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

    Ok(Some(SendGuard {
        shared: shared.clone(),
    }))
}

pub(crate) fn install<'v>(builder: &mut dolang::runtime::vm::Builder<'v>) {
    builder.pipe_handler(move |strand, out_send, out_recv| {
        make_pair(strand, out_send, out_recv);
    });
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

pub(crate) fn make_pair<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut out_send: Slot<'v, '_>,
    mut out_recv: Slot<'v, '_>,
) {
    let vm = strand.vm();
    let inner = Rc::new(RefCell::new(PipeChannelShared::new()));
    let global = vm.state::<Global<'v>>();
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

    let send_inst = global.types.pipe_sender.downcast(&out_send).unwrap();
    let mut send_borrow = send_inst.borrow_mut_unwrap();
    Output::set(strand, Mut::slot_mut::<0>(&mut send_borrow), &out_recv);
}

fn encode_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    channel_mode: ChannelMode,
) -> Result<'v, 's, Vec<u8>> {
    match value.view(strand) {
        View::Str(s) => {
            let str: String = s.into();
            let mut bytes: Vec<u8> = str.into();
            if channel_mode == ChannelMode::Line {
                bytes.push(b'\n');
            }
            Ok(bytes)
        }
        View::Bin(s) => Ok(s.into()),
        _ => {
            let mut format = BytesFormat(Vec::new());
            value.display(strand, &mut format)?;
            if channel_mode == ChannelMode::Line {
                format.0.push(b'\n');
            }
            Ok(format.0)
        }
    }
}

impl<'v> Object<'v> for PipeReceiver {
    const MODULE: &'v str = "proc";
    const NAME: &'v str = "PipeReceiver";
    const SLOTS: usize = 3;
    type Annex = PipeAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter).method(
            "close",
            async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.annex().shared.borrow_mut().close_recv();
                Ok(())
            },
        )
    }

    async fn input<'a, 's>(
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let shared = &this.annex().shared;
        let channel_mode = this.annex().global.local.get(strand).channel_mode();

        loop {
            let recv_end = {
                let mut inner = shared.borrow_mut();
                if inner.recv_closed {
                    return Err(Error::state_error(strand, "channel end is closed"));
                }
                let send_closed = inner.send_closed;
                match &mut inner.state {
                    PipeState::Value => {
                        if !matches!(inner.buffered, BufferedValue::Empty) {
                            inner.buffered = BufferedValue::Empty;
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
                match channel_mode {
                    ChannelMode::Chunk => {
                        let mut buf = vec![0u8; 8192];
                        match reader.read(&mut buf).await {
                            Ok(0) => {
                                reader.discard();
                                let mut inner = shared.borrow_mut();
                                inner.state = PipeState::Value;
                                inner.wake_senders();
                                inner.wake_receivers();
                                inner.wake_negotiators();
                                continue;
                            }
                            Ok(n) => {
                                buf.truncate(n);
                                Output::set(strand, out, buf.as_slice());
                                return Ok(true);
                            }
                            Err(e) => {
                                return Err(e.into_sys(strand));
                            }
                        }
                    }
                    ChannelMode::Line => {
                        let mut line = String::new();
                        match reader.read_line(&mut line).await {
                            Ok(0) => {
                                reader.discard();
                                let mut inner = shared.borrow_mut();
                                inner.state = PipeState::Value;
                                inner.wake_senders();
                                inner.wake_receivers();
                                inner.wake_negotiators();
                                continue;
                            }
                            Ok(_) => {
                                let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                                Output::set(strand, out, trimmed.as_str());
                                return Ok(true);
                            }
                            Err(e) => {
                                return Err(e.into_sys(strand));
                            }
                        }
                    }
                }
            }

            future::poll_fn(|cx| {
                let mut inner = shared.borrow_mut();
                let ready = match &inner.state {
                    PipeState::Value => {
                        !matches!(inner.buffered, BufferedValue::Empty)
                            || inner.send_closed
                            || inner.recv_closed
                    }
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
        builder.supertype(TypeObject::Sink).method(
            "close",
            async move |this, strand, args, _out| {
                let ([], [err, backtrace]) = unpack!(strand, args, 0, 1, backtrace_sym = None)?;
                let shared = &this.annex().shared;
                let send_borrow = this.borrow(strand)?;
                let recv_inst = this
                    .annex()
                    .global
                    .types
                    .pipe_receiver
                    .downcast(Ref::slot::<0>(&send_borrow))
                    .unwrap();
                let mut recv_borrow = recv_inst.borrow_mut(strand)?;
                if let Some(err) = err {
                    if let Some(backtrace) = backtrace {
                        if backtrace.as_backtrace(strand.vm()).is_none() {
                            return Err(Error::type_error(strand, "expected strand.Backtrace"));
                        }
                        Output::set(strand, Mut::slot_mut::<2>(&mut recv_borrow), backtrace);
                    } else {
                        Output::set(strand, Mut::slot_mut::<2>(&mut recv_borrow), Nil);
                    }
                    Output::set(strand, Mut::slot_mut::<1>(&mut recv_borrow), err);
                }
                shared.borrow_mut().close_send();
                Ok(())
            },
        )
    }

    async fn output<'a, 's>(
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
        let channel_mode = global.local.get(strand).channel_mode();

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
                        if !matches!(inner.buffered, BufferedValue::Empty) {
                            None
                        } else {
                            inner.buffered = if channel_mode == ChannelMode::Chunk {
                                BufferedValue::Chunk
                            } else {
                                BufferedValue::Line
                            };
                            drop(inner);
                            let send_borrow = this.borrow(strand)?;
                            let recv_inst = global
                                .types
                                .pipe_receiver
                                .downcast(Ref::slot::<0>(&send_borrow))
                                .unwrap();
                            let mut recv_borrow = recv_inst.borrow_mut(strand)?;
                            Output::set(strand, Mut::slot_mut::<0>(&mut recv_borrow), value);
                            drop(recv_borrow);
                            drop(send_borrow);
                            shared.borrow_mut().wake_receivers();
                            return Ok(());
                        }
                    }
                    PipeState::RecvPipe => {
                        if inner.send_end.is_present() {
                            drop(inner);
                            Some(SendEndGuard::take(shared).into_sys(strand)?)
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
                let bytes = encode_value(strand, value, channel_mode)?;
                match writer.write_all(&bytes).await {
                    Ok(()) => {
                        return Ok(());
                    }
                    Err(e) => {
                        return Err(e.into_sys(strand));
                    }
                }
            }

            future::poll_fn(|cx| {
                let mut inner = shared.borrow_mut();
                let ready = match &inner.state {
                    PipeState::Value => {
                        matches!(inner.buffered, BufferedValue::Empty)
                            || inner.send_closed
                            || inner.recv_closed
                    }
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
