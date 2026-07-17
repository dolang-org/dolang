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
    gc::{self, Collect, arena::Visit},
    strand::{Strand, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, iter,
    protocol::{GcObj, Header, Protocol, Recv},
};

enum RecvResult<'v> {
    Ok(Value<'v>),
    Poisoned(Value<'v>, Value<'v>),
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
        strand: &'s StrandInner<'v>,
        recv: gc::Borrow<'v, 'a, Header, Receiver<'v>>,
        out: Slot<'v, 'a>,
    },
}

enum SendFuture<'v, 's, 'a> {
    Ready(Result<'v, 's, ()>),
    Pend {
        strand: &'s StrandInner<'v>,
        send: gc::Borrow<'v, 'a, Header, Sender<'v>>,
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
    fn recv(&mut self, cx: Option<&mut task::Context<'_>>) -> RecvResult<'v> {
        if self.recv_closed {
            RecvResult::Closed
        } else if let Some(value) = self.queue.pop_front() {
            if let Some(wake) = self.senders.pop_front() {
                wake.wake();
            }
            RecvResult::Ok(value)
        } else if self.send_closed {
            match &self.poison {
                Some((value, backtrace)) => RecvResult::Poisoned(value.dup(), backtrace.dup()),
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
            self.queue.push_back(value.dup());
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
                    return Poll::Ready(Err(Error::concurrency_raw(strand)));
                };
                let res = borrow.recv(Some(cx));
                mem::drop(borrow);
                match res {
                    RecvResult::Ok(value) => {
                        out.store(value);
                        Poll::Ready(Ok(true))
                    }
                    RecvResult::Poisoned(value, backtrace) => Poll::Ready(Err(
                        match Error::from_value_backtrace_raw(strand, value, backtrace) {
                            Ok(err) => err,
                            Err(err) => err,
                        },
                    )),
                    RecvResult::Pending => Poll::Pending,
                    RecvResult::Stop => Poll::Ready(Ok(false)),
                    RecvResult::Closed => {
                        Poll::Ready(Err(Error::runtime_raw(strand, "receive on closed channel")))
                    }
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
                let Some(mut borrow) = send.get().receiver.borrow_mut() else {
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
    const IMMUTABLE: bool = false;
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
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, &strand.singletons().input_iter)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn crate::value::Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<channel recv>")
    }

    fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, bool>> {
        let Ok(mut borrow) = this.borrow_mut(strand) else {
            return RecvFuture::Ready(Err(Error::concurrency(strand)));
        };
        let res = borrow.recv(None);
        mem::drop(borrow);
        match res {
            RecvResult::Ok(value) => {
                out.store(value);
                RecvFuture::Ready(Ok(true))
            }
            RecvResult::Poisoned(value, backtrace) => {
                RecvFuture::Ready(Err(Error::from_value_backtrace(strand, &value, &backtrace)
                    .expect("invalid backtrace")))
            }
            RecvResult::Pending => RecvFuture::Pend {
                strand: strand.inner,
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
        Output::set(strand, out, &this);
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
                let ([], []) = unpack!(strand, args, 0, 0)?;
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
}

pub(crate) struct Sender<'v> {
    receiver: GcObj<'v, Receiver<'v>>,
}

unsafe impl<'v> Collect for Sender<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.receiver.accept(visit)
    }

    fn clear(&mut self) {
        // Can't clear `receiver`, but receiver itself can be cleared
    }
}

impl<'v> Protocol<'v> for Sender<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.singletons().sinkable)
            || supertype.eq(strand, &strand.singletons().output_iter)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().output_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn crate::value::Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<channel send>")
    }

    fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        let receiver = &this.get().receiver;
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
        Output::set(strand, out, &this);
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
                let backtrace_key = Sym::well_known(sym::BACKTRACE);
                let ([], [err, backtrace]) = unpack!(strand, args, 0, 1, backtrace_key = None)?;
                let poison_backtrace = match backtrace {
                    Some(backtrace) => {
                        let Some(_) = backtrace.downcast_ref(strand.builtin_types().backtrace)
                        else {
                            return Err(Error::type_error(strand, "expected strand.Backtrace"));
                        };
                        Some(backtrace)
                    }
                    None => None,
                };
                let poison_err = err.map(|mut err| err.take());
                let borrow = this.borrow(strand)?;
                let receiver = borrow.receiver.clone();
                mem::drop(borrow);

                let mut borrow = receiver
                    .borrow_mut()
                    .ok_or_else(|| Error::concurrency(strand))?;
                borrow.send_closed = true;
                if let Some(poison_err) = poison_err {
                    borrow.poison = Some((
                        poison_err,
                        poison_backtrace.map(|mut s| s.take()).unwrap_or(Value::NIL),
                    ));
                }
                for wake in borrow.receivers.drain(0..) {
                    wake.wake()
                }
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

pub(crate) fn pair<'v>(
    vm: &Vm<'v>,
    limit: usize,
) -> (GcObj<'v, Sender<'v>>, GcObj<'v, Receiver<'v>>) {
    let receiver = Receiver {
        queue: VecDeque::new(),
        senders: VecDeque::new(),
        receivers: VecDeque::new(),
        send_closed: false,
        recv_closed: false,
        poison: None,
        limit,
    };
    let receiver = GcObj::new(vm.arena(), vm.builtin_types().channel_recv, receiver);
    let sender = Sender {
        receiver: receiver.clone(),
    };
    let sender = GcObj::new(vm.arena(), vm.builtin_types().channel_send, sender);
    (sender, receiver)
}
