use std::{
    collections::VecDeque,
    mem,
    ops::ControlFlow,
    pin::Pin,
    task::{self, Poll, Waker},
};

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit, with_pin},
    strand::{Strand, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Value},
    vm::{Alloc, AllocExt},
};

use super::{
    BoundMethod, iter,
    protocol::{GcObj, Header, Protocol, Recv},
};

enum RecvResult {
    Ok,
    Poisoned,
    Pending,
    Closed,
    Stop,
}

enum SendResult {
    Ok,
    Pending,
    Closed,
    Stop,
}

enum RecvFuture<'v, 's, 'a> {
    Ready(Result<'v, 's, bool>),
    Pend {
        strand: &'a mut Strand<'v, 's>,
        recv: gc::Pin<'v, 'a, Header, Receiver<'v>>,
        out: Slot<'v, 'a>,
    },
}

enum SendFuture<'v, 's, 'a> {
    Ready(Result<'v, 's, ()>),
    Pend {
        strand: &'s StrandInner<'v>,
        send: gc::Pin<'v, 'a, Header, SenderObj<'v>>,
        value: Slot<'v, 'a>,
    },
}

pub(crate) struct Receiver<'v> {
    queue: VecDeque<Value<'v>>,
    senders: VecDeque<Waker>,
    receivers: VecDeque<Waker>,
    send_closed: bool,
    recv_closed: bool,
    poison: Option<(Value<'v>, Value<'v>)>,
    limit: usize,
}

impl<'v> Receiver<'v> {
    fn recv(
        &mut self,
        alloc: &mut dyn Alloc<'v>,
        cx: Option<&mut task::Context<'_>>,
        out: Slot<'v, '_>,
    ) -> RecvResult {
        if self.recv_closed {
            RecvResult::Closed
        } else if let Some(value) = self.queue.pop_front() {
            alloc.store(out, value);
            if let Some(wake) = self.senders.pop_front() {
                wake.wake();
            }
            RecvResult::Ok
        } else if self.send_closed {
            match &self.poison {
                Some(_) => RecvResult::Poisoned,
                None => RecvResult::Stop,
            }
        } else {
            if let Some(cx) = cx {
                self.receivers.push_back(cx.waker().clone());
            }
            RecvResult::Pending
        }
    }

    fn send(&mut self, value: &Value<'v>, cx: Option<&mut task::Context<'_>>) -> SendResult {
        if self.send_closed {
            SendResult::Closed
        } else if self.recv_closed {
            SendResult::Stop
        } else if self.queue.len() < self.limit {
            // SAFETY: the copy moves straight into the scanned queue
            self.queue.push_back(unsafe { value.dup() });
            if let Some(wake) = self.receivers.pop_front() {
                wake.wake();
            }
            SendResult::Ok
        } else {
            if let Some(cx) = cx {
                self.senders.push_back(cx.waker().clone());
            }
            SendResult::Pending
        }
    }
}

impl<'v, 's, 'a> Future for RecvFuture<'v, 's, 'a> {
    type Output = Result<'v, 's, bool>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            RecvFuture::Ready(res) => Poll::Ready(mem::replace(res, Ok(false))),
            RecvFuture::Pend { strand, recv, out } => {
                let Some(mut borrow) = recv.borrow_mut() else {
                    return Poll::Ready(Err(Error::concurrency_raw(strand.inner)));
                };
                let res = borrow.recv(*strand, Some(cx), Slot::reborrow(out));
                if let RecvResult::Poisoned = res {
                    let (value, backtrace) = borrow.poison.as_ref().unwrap();
                    let err = Error::from_value_backtrace(strand, value, backtrace);
                    return Poll::Ready(Err(err.unwrap_or_else(|e| e)));
                }
                mem::drop(borrow);
                match res {
                    RecvResult::Ok => Poll::Ready(Ok(true)),
                    RecvResult::Poisoned => unreachable!(),
                    RecvResult::Pending => Poll::Pending,
                    RecvResult::Stop => Poll::Ready(Ok(false)),
                    RecvResult::Closed => Poll::Ready(Err(Error::runtime_raw(
                        strand.inner,
                        "receive on closed channel",
                    ))),
                }
            }
        }
    }
}

impl<'v, 's, 'a> Future for SendFuture<'v, 's, 'a> {
    type Output = Result<'v, 's, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            SendFuture::Ready(res) => Poll::Ready(mem::replace(res, Ok(()))),
            SendFuture::Pend {
                strand,
                send,
                value,
            } => {
                // SAFETY: a transient pin; polling has no strand, so it cannot collect
                let receiver = unsafe { send.get().receiver.pin_unchecked() };
                let Some(mut borrow) = receiver.borrow_mut() else {
                    return Poll::Ready(Err(Error::concurrency_raw(strand)));
                };
                let res = borrow.send(value, Some(cx));
                mem::drop(borrow);
                match res {
                    SendResult::Ok => Poll::Ready(Ok(())),
                    SendResult::Pending => Poll::Pending,
                    SendResult::Stop => Poll::Ready(Err(Error::output_stop_raw(strand))),
                    SendResult::Closed => {
                        Poll::Ready(Err(Error::runtime_raw(strand, "send on closed channel")))
                    }
                }
            }
        }
    }
}

unsafe impl<'v> Collect for Receiver<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for item in self.queue.iter() {
            item.accept(visit)?
        }
        if let Some((value, backtrace)) = &self.poison {
            value.accept(visit)?;
            backtrace.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.send_closed = true;
        self.recv_closed = true;
        self.poison = None;
        for wake in self.senders.drain(0..).chain(self.receivers.drain(0..)) {
            wake.wake();
        }
    }
}

impl<'v> Drop for Receiver<'v> {
    fn drop(&mut self) {
        self.clear()
    }
}

impl<'v> Protocol<'v> for Receiver<'v> {
    const MODULE: &'v str = "strand";
    const NAME: &'v str = "Receiver";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, bool>> {
        let Ok(mut borrow) = this.borrow_mut(strand) else {
            return RecvFuture::Ready(Err(Error::concurrency(strand)));
        };
        let res = borrow.recv(strand, None, Slot::reborrow(&mut out));
        if let RecvResult::Poisoned = res {
            let (value, backtrace) = borrow.poison.as_ref().unwrap();
            let err = Error::from_value_backtrace(strand, value, backtrace);
            return RecvFuture::Ready(Err(err.expect("invalid backtrace")));
        }
        mem::drop(borrow);
        match res {
            RecvResult::Ok => RecvFuture::Ready(Ok(true)),
            RecvResult::Poisoned => unreachable!(),
            RecvResult::Pending => RecvFuture::Pend {
                strand,
                recv: this.receiver,
                out,
            },
            RecvResult::Closed => {
                RecvFuture::Ready(Err(Error::state_error(strand, "channel closed")))
            }
            RecvResult::Stop => RecvFuture::Ready(Ok(false)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CLOSE => {
                unpack!(strand, args =>);
                let mut borrow = this.borrow_mut(strand)?;
                borrow.recv_closed = true;
                borrow.queue.clear();
                borrow.poison = None;
                for wake in borrow.senders.drain(0..) {
                    wake.wake()
                }
                Ok(())
            }
            _ => iter::iter_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::CLOSE {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iter::iter_get(strand, &this, field, out)
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
    }
}

pub(crate) struct Sender<'v> {
    receiver: GcObj<'v, Receiver<'v>>,
}

unsafe impl<'v> gc::ImmState for Sender<'v> {
    const CYCLIC: bool = true;
}

impl<'v> gc::Annex for Sender<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.receiver.accept(visit)
    }
}

pub(crate) type SenderObj<'v> = gc::Imm<Sender<'v>>;

impl<'v> Protocol<'v> for SenderObj<'v> {
    const MODULE: &'v str = "strand";
    const NAME: &'v str = "Sender";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter)
    }

    fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        // SAFETY: a transient pin; sending cannot collect
        let receiver = unsafe { this.get().receiver.pin_unchecked() };
        let Some(mut borrow) = receiver.borrow_mut() else {
            return SendFuture::Ready(Err(Error::concurrency(strand)));
        };
        let res = borrow.send(&value, None);
        mem::drop(borrow);
        match res {
            SendResult::Ok => SendFuture::Ready(Ok(())),
            SendResult::Pending => SendFuture::Pend {
                strand: strand.inner,
                send: this.receiver,
                value,
            },
            SendResult::Stop => SendFuture::Ready(Err(Error::sink_stop(strand))),
            SendResult::Closed => {
                SendFuture::Ready(Err(Error::state_error(strand, "send on closed channel")))
            }
        }
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CLOSE => {
                let backtrace = Sym::well_known(sym::BACKTRACE);
                unpack!(strand, args => ?err, ?:backtrace);
                let poison_backtrace = match backtrace {
                    Some(backtrace) => {
                        let Some(_) = backtrace.downcast_ref(strand.builtin_types().backtrace)
                        else {
                            return Err(Error::type_error(strand, "expected inspect.Backtrace"));
                        };
                        Some(backtrace)
                    }
                    None => None,
                };
                with_pin!(strand, this.get().receiver, |receiver| {
                    let mut borrow = receiver
                        .borrow_mut()
                        .ok_or_else(|| Error::concurrency(strand))?;
                    borrow.send_closed = true;
                    if let Some(mut err) = err {
                        // SAFETY: the values move straight into the borrowed receiver
                        let poison = unsafe {
                            let backtrace = poison_backtrace
                                .map_or(Value::NIL, |mut backtrace| backtrace.take(strand));
                            (err.take(strand), backtrace)
                        };
                        borrow.poison = Some(poison);
                    }
                    for wake in borrow.receivers.drain(0..) {
                        wake.wake()
                    }
                    Ok(())
                })?;
                Ok(())
            }
            _ => iter::sink_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::CLOSE => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::sink_get(strand, &this, field, out),
        }
    }
}

/// Create a channel with room for `limit` queued values, storing its sending end into
/// `send` and its receiving end into `recv`.
///
/// The receiver is created into `recv` first, then copied from there into the sender
/// once the sender is allocated.
pub(crate) fn pair<'v>(
    alloc: &mut impl Alloc<'v>,
    limit: usize,
    send: impl Output<'v>,
    mut recv: impl Output<'v>,
) {
    let types = alloc.alloc_vm(crate::vm::private::Sealed).builtin_types();
    let mut recv = Slot::from_output(&mut recv);
    let receiver = Receiver {
        queue: VecDeque::new(),
        senders: VecDeque::new(),
        receivers: VecDeque::new(),
        send_closed: false,
        recv_closed: false,
        poison: None,
        limit,
    };
    types
        .channel_recv
        .create(alloc, receiver, Slot::reborrow(&mut recv));
    let embryo = types.channel_send.embryo(alloc);
    let receiver = recv
        .downcast_ref(types.channel_recv)
        .expect("channel receiver");
    // SAFETY: the sender is allocated; the receiver, rooted in `recv`, is copied straight
    // into its payload
    let sender = Sender {
        receiver: unsafe { receiver.dup() },
    };
    embryo.finish_imm(alloc, sender, send);
}
