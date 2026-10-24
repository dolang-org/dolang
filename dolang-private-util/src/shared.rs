//! A cloneable futures-io handle over one stream.
//!
//! A [`Shared`] holds its stream directly until it is first cloned. From then on the stream sits
//! behind a mutex that every clone shares, and each operation holds the lock from its first poll
//! until it completes, so no clone touches the stream mid-operation or replaces the waker it
//! stored. Once the other clones are gone, the next operation takes the stream back out.
//!
//! [`Shared::try_lock`] holds the stream through a shared reference when that needs no
//! waiting, for callers that only have one.

use std::{
    cell::{RefCell, RefMut},
    error::Error,
    fmt,
    future::{Future, poll_fn},
    io::{self, IoSlice, IoSliceMut, SeekFrom},
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use futures::{
    io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite},
    lock::{Mutex, OwnedMutexGuard, OwnedMutexLockFuture},
};

/// The error an operation on a closed [`Shared`] carries.
#[derive(Debug)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stream closed")
    }
}

impl Error for Closed {}

/// A stream shared among clones.
///
/// A handle is `Send` when `T` is, but not `Sync`: clones, not references, cross threads.
/// `poll_close` only flushes; [`close`](Self::close) is what ends the stream for every handle.
pub struct Shared<T> {
    state: RefCell<State<T>>,
}

enum State<T> {
    /// The only handle; `busy` records an operation in progress, so a clone made meanwhile
    /// takes the lock on this handle's behalf.
    Plain { io: Option<T>, busy: bool },
    /// `guard` is held for the operation in progress, and `waiting` while the lock is contended.
    Wrapped {
        inner: Arc<Mutex<Option<T>>>,
        guard: Option<OwnedMutexGuard<Option<T>>>,
        waiting: Option<OwnedMutexLockFuture<Option<T>>>,
    },
}

impl<T> Shared<T> {
    pub fn new(io: T) -> Self {
        Self {
            state: RefCell::new(State::Plain {
                io: Some(io),
                busy: false,
            }),
        }
    }

    /// Closes the stream for every handle and returns it, for the caller to finish or drop.
    ///
    /// It waits for an operation in progress on another handle. Returns `None` if the stream
    /// was already closed.
    pub async fn close(&mut self) -> Option<T> {
        poll_fn(|cx| {
            let taken = match self.state.get_mut() {
                State::Plain { io, busy } => {
                    *busy = false;
                    io.take()
                }
                State::Wrapped {
                    inner,
                    guard,
                    waiting,
                } => {
                    let held = ready!(poll_lock(inner, guard, waiting, cx));
                    let taken = held.take();
                    *guard = None;
                    taken
                }
            };
            Poll::Ready(taken)
        })
        .await
    }

    /// Holds the stream for several operations in a row, until the guard drops.
    ///
    /// It waits for an operation in progress on another handle, and fails if the stream is
    /// closed.
    pub async fn lock(&mut self) -> io::Result<Locked<'_, T>> {
        let res = poll_fn(|cx| self.poll_acquire(cx).map_ok(|_| ())).await;
        self.settle(res.is_err());
        res.map(|()| Locked { shared: self })
    }

    /// Holds the stream without waiting, through a shared reference.
    ///
    /// Returns `None` if the stream is closed, if an operation is in progress on this or
    /// another handle, or if this handle is already held. Cloning this handle while the guard
    /// is held panics.
    pub fn try_lock(&self) -> Option<TryLocked<'_, T>> {
        let state = self.state.try_borrow_mut().ok()?;
        let held = match &*state {
            State::Plain {
                io: Some(_),
                busy: false,
            } => None,
            State::Wrapped {
                inner,
                guard: None,
                waiting: None,
            } => Some(inner.try_lock_owned().filter(|held| held.is_some())?),
            _ => return None,
        };
        let held = match held {
            None => TryHeld::Plain(state),
            Some(held) => TryHeld::Wrapped(held, PhantomData),
        };
        Some(TryLocked { held })
    }

    #[cfg(test)]
    fn is_shared(&self) -> bool {
        matches!(*self.state.borrow(), State::Wrapped { .. })
    }

    /// Readies the stream for an operation on this handle, and returns it.
    fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&mut T>> {
        let state = self.state.get_mut();
        if let State::Wrapped {
            inner,
            guard: None,
            waiting: None,
        } = state
            && Arc::strong_count(inner) == 1
        {
            let State::Wrapped { inner, .. } = mem::replace(
                state,
                State::Plain {
                    io: None,
                    busy: false,
                },
            ) else {
                unreachable!()
            };
            // No other handle exists, and none can be cloned from one.
            let inner = Arc::into_inner(inner).expect("the only reference");
            *state = State::Plain {
                io: inner.into_inner(),
                busy: false,
            };
        }
        let io = match state {
            State::Plain { io, .. } => io.as_mut(),
            State::Wrapped {
                inner,
                guard,
                waiting,
            } => ready!(poll_lock(inner, guard, waiting, cx)).as_mut(),
        };
        Poll::Ready(io.ok_or_else(|| io::Error::other(Closed)))
    }

    /// Records whether the operation on this handle is done, releasing the lock if so.
    fn settle(&mut self, done: bool) {
        match self.state.get_mut() {
            State::Plain { busy, .. } => *busy = !done,
            State::Wrapped { guard, .. } => {
                if done {
                    *guard = None;
                }
            }
        }
    }

    fn poll_op<R>(
        &mut self,
        cx: &mut Context<'_>,
        op: impl FnOnce(Pin<&mut T>, &mut Context<'_>) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>>
    where
        T: Unpin,
    {
        let res = match ready!(self.poll_acquire(cx)) {
            Ok(io) => op(Pin::new(io), cx),
            Err(err) => Poll::Ready(Err(err)),
        };
        self.settle(res.is_ready());
        res
    }
}

/// A stream held by [`Shared::lock`].
pub struct Locked<'a, T> {
    shared: &'a mut Shared<T>,
}

impl<T> Deref for Locked<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard borrows the handle exclusively, so the cell has no other borrow.
        let state = unsafe { &*self.shared.state.as_ptr() };
        match state {
            State::Plain { io, .. } => io.as_ref(),
            State::Wrapped { guard, .. } => guard.as_ref().and_then(|held| held.as_ref()),
        }
        .expect("the stream is held")
    }
}

impl<T> DerefMut for Locked<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self.shared.state.get_mut() {
            State::Plain { io, .. } => io.as_mut(),
            State::Wrapped { guard, .. } => guard.as_mut().and_then(|held| held.as_mut()),
        }
        .expect("the stream is held")
    }
}

impl<T> Drop for Locked<'_, T> {
    fn drop(&mut self) {
        self.shared.settle(true);
    }
}

/// A stream held by [`Shared::try_lock`].
pub struct TryLocked<'a, T> {
    held: TryHeld<'a, T>,
}

enum TryHeld<'a, T> {
    /// The handle's own state, borrowed for as long as the guard lives.
    Plain(RefMut<'a, State<T>>),
    Wrapped(OwnedMutexGuard<Option<T>>, PhantomData<&'a Shared<T>>),
}

impl<T> Deref for TryLocked<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match &self.held {
            TryHeld::Plain(state) => match &**state {
                State::Plain { io, .. } => io.as_ref(),
                State::Wrapped { .. } => None,
            },
            TryHeld::Wrapped(held, _) => held.as_ref(),
        }
        .expect("the stream is held")
    }
}

impl<T> DerefMut for TryLocked<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.held {
            TryHeld::Plain(state) => match &mut **state {
                State::Plain { io, .. } => io.as_mut(),
                State::Wrapped { .. } => None,
            },
            TryHeld::Wrapped(held, _) => held.as_mut(),
        }
        .expect("the stream is held")
    }
}

/// Holds the lock for this handle, waiting for it if another handle has it.
fn poll_lock<'a, T>(
    inner: &Arc<Mutex<T>>,
    guard: &'a mut Option<OwnedMutexGuard<T>>,
    waiting: &mut Option<OwnedMutexLockFuture<T>>,
    cx: &mut Context<'_>,
) -> Poll<&'a mut T> {
    if guard.is_none() {
        let acquired = match waiting {
            Some(future) => ready!(Pin::new(future).poll(cx)),
            None => match inner.try_lock_owned() {
                Some(acquired) => acquired,
                None => {
                    let future = waiting.insert(Arc::clone(inner).lock_owned());
                    ready!(Pin::new(future).poll(cx))
                }
            },
        };
        *waiting = None;
        *guard = Some(acquired);
    }
    Poll::Ready(guard.as_mut().expect("the lock is held"))
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        let mut state = self.state.borrow_mut();
        if let State::Plain { io, busy } = &mut *state {
            let inner = Arc::new(Mutex::new(io.take()));
            let guard = busy.then(|| inner.try_lock_owned().expect("the mutex is new"));
            *state = State::Wrapped {
                inner,
                guard,
                waiting: None,
            };
        }
        let State::Wrapped { inner, .. } = &*state else {
            unreachable!()
        };
        Self {
            state: RefCell::new(State::Wrapped {
                inner: Arc::clone(inner),
                guard: None,
                waiting: None,
            }),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Shared<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_op(cx, |io, cx| io.poll_read(cx, buf))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .poll_op(cx, |io, cx| io.poll_read_vectored(cx, bufs))
    }
}

/// A `poll_fill_buf` holds the lock until `consume`, since the buffer borrows from the stream.
/// If the stream fails to fill, the lock stays held until this handle's next operation.
impl<T: AsyncBufRead + Unpin> AsyncBufRead for Shared<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        this.settle(false);
        if let Err(err) = ready!(this.poll_acquire(cx)) {
            this.settle(true);
            return Poll::Ready(Err(err));
        }
        // Held now, so acquiring again is immediate.
        let io = ready!(this.poll_acquire(cx)).expect("the stream is open");
        Pin::new(io).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        let io = match this.state.get_mut() {
            State::Plain { io, .. } => io.as_mut(),
            State::Wrapped { guard, .. } => guard.as_mut().and_then(|held| held.as_mut()),
        };
        if let Some(io) = io {
            Pin::new(io).consume(amt);
        }
        this.settle(true);
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Shared<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_op(cx, |io, cx| io.poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .poll_op(cx, |io, cx| io.poll_write_vectored(cx, bufs))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_op(cx, |io, cx| io.poll_flush(cx))
    }

    /// Only flushes: other handles may still use the stream.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_op(cx, |io, cx| io.poll_flush(cx))
    }
}

impl<T: AsyncSeek + Unpin> AsyncSeek for Shared<T> {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        self.get_mut().poll_op(cx, |io, cx| io.poll_seek(cx, pos))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Wake, Waker},
    };

    use futures::io::{BufReader, Cursor};

    use super::*;

    /// A waker that records being woken.
    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn flag() -> (Arc<Flag>, Waker) {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        (Arc::clone(&flag), Waker::from(flag))
    }

    #[derive(Default)]
    struct GateState {
        open: bool,
        waker: Option<Waker>,
        flushes: usize,
        closes: usize,
    }

    /// A stream whose reads wait until the gate opens, and which records what it was asked.
    #[derive(Clone, Default)]
    struct Gate(Arc<StdMutex<GateState>>);

    impl Gate {
        fn open(&self) {
            let mut state = self.0.lock().unwrap();
            state.open = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }

        fn stored_waker(&self) -> Waker {
            self.0
                .lock()
                .unwrap()
                .waker
                .clone()
                .expect("a stored waker")
        }
    }

    impl AsyncRead for Gate {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let mut state = self.0.lock().unwrap();
            if !state.open {
                state.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            buf[0] = b'x';
            Poll::Ready(Ok(1))
        }
    }

    impl AsyncWrite for Gate {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.lock().unwrap().flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.lock().unwrap().closes += 1;
            Poll::Ready(Ok(()))
        }
    }

    fn read<T: AsyncRead + Unpin>(
        io: &mut Shared<T>,
        waker: &Waker,
        len: usize,
    ) -> Poll<io::Result<Vec<u8>>> {
        let mut buf = vec![0; len];
        let mut cx = Context::from_waker(waker);
        let n = ready!(Pin::new(io).poll_read(&mut cx, &mut buf))?;
        buf.truncate(n);
        Poll::Ready(Ok(buf))
    }

    fn close<T>(io: &mut Shared<T>, waker: &Waker) -> Poll<Option<T>> {
        pin!(io.close()).poll(&mut Context::from_waker(waker))
    }

    fn assert_closed<T>(res: Poll<io::Result<T>>) {
        let Poll::Ready(Err(err)) = res else {
            panic!("expected a closed error");
        };
        assert!(err.get_ref().is_some_and(|inner| inner.is::<Closed>()));
    }

    #[test]
    fn plain_handle_passes_through() {
        fn assert_send<T: Send>() {}
        assert_send::<Shared<Cursor<Vec<u8>>>>();

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut io = Shared::new(Cursor::new(b"abcdef".to_vec()));
        assert!(matches!(read(&mut io, waker, 2), Poll::Ready(Ok(ref data)) if data == b"ab"));
        let pos = Pin::new(&mut io).poll_seek(&mut cx, SeekFrom::Start(4));
        assert!(matches!(pos, Poll::Ready(Ok(4))));
        let written = Pin::new(&mut io).poll_write(&mut cx, b"EF");
        assert!(matches!(written, Poll::Ready(Ok(2))));
        assert!(!io.is_shared());
        let Poll::Ready(Some(cursor)) = close(&mut io, waker) else {
            panic!("expected the stream");
        };
        assert_eq!(cursor.into_inner(), b"abcdEF");
    }

    #[test]
    fn clones_share_one_cursor() {
        let waker = Waker::noop();
        let mut a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let mut b = a.clone();
        assert!(a.is_shared());
        assert!(matches!(read(&mut a, waker, 2), Poll::Ready(Ok(ref data)) if data == b"ab"));
        assert!(matches!(read(&mut b, waker, 2), Poll::Ready(Ok(ref data)) if data == b"cd"));
    }

    #[test]
    fn operation_holds_the_lock() {
        let gate = Gate::default();
        let mut a = Shared::new(gate.clone());
        let mut b = a.clone();
        let (_, waker_a) = flag();
        let (woken_b, waker_b) = flag();
        assert!(read(&mut a, &waker_a, 1).is_pending());
        assert!(read(&mut b, &waker_b, 1).is_pending());
        // `b` never reached the stream, so the stream still holds `a`'s waker.
        assert!(gate.stored_waker().will_wake(&waker_a));
        gate.open();
        assert!(read(&mut a, &waker_a, 1).is_ready());
        assert!(woken_b.0.load(Ordering::SeqCst));
        assert!(matches!(read(&mut b, &waker_b, 1), Poll::Ready(Ok(_))));
    }

    #[test]
    fn clone_waits_for_the_original_operation() {
        let gate = Gate::default();
        let mut a = Shared::new(gate.clone());
        let (_, waker_a) = flag();
        assert!(read(&mut a, &waker_a, 1).is_pending());
        let mut b = a.clone();
        assert!(read(&mut b, Waker::noop(), 1).is_pending());
        assert!(gate.stored_waker().will_wake(&waker_a));
        gate.open();
        assert!(read(&mut a, &waker_a, 1).is_ready());
        assert!(read(&mut b, Waker::noop(), 1).is_ready());
    }

    #[test]
    fn fill_buf_holds_the_lock_until_consume() {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut a = Shared::new(BufReader::new(Cursor::new(b"abcdef".to_vec())));
        let mut b = a.clone();
        let Poll::Ready(Ok(buf)) = Pin::new(&mut a).poll_fill_buf(&mut cx) else {
            panic!("expected a buffer");
        };
        assert_eq!(buf, b"abcdef");
        assert!(read(&mut b, waker, 2).is_pending());
        Pin::new(&mut a).consume(2);
        assert!(matches!(read(&mut b, waker, 2), Poll::Ready(Ok(ref data)) if data == b"cd"));
    }

    #[test]
    fn lock_holds_the_stream_until_dropped() {
        let waker = Waker::noop();
        let mut a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let mut b = a.clone();
        let mut locked = pin!(a.lock());
        let Poll::Ready(Ok(mut held)) = locked.as_mut().poll(&mut Context::from_waker(waker))
        else {
            panic!("expected the stream");
        };
        held.set_position(2);
        assert!(read(&mut b, waker, 2).is_pending());
        drop(held);
        assert!(matches!(read(&mut b, waker, 2), Poll::Ready(Ok(ref data)) if data == b"cd"));
    }

    #[test]
    fn try_lock_holds_a_plain_handle() {
        let a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let mut held = a.try_lock().expect("the stream");
        held.set_position(2);
        assert!(a.try_lock().is_none());
        drop(held);
        assert_eq!(a.try_lock().expect("the stream").position(), 2);
        assert!(!a.is_shared());
    }

    #[test]
    fn try_lock_is_seen_by_clones() {
        let a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let mut b = a.clone();
        a.try_lock().expect("the stream").set_position(2);
        assert!(
            matches!(read(&mut b, Waker::noop(), 2), Poll::Ready(Ok(ref data)) if data == b"cd")
        );
    }

    #[test]
    fn try_lock_fails_while_another_handle_holds_the_stream() {
        let gate = Gate::default();
        let a = Shared::new(gate.clone());
        let mut b = a.clone();
        assert!(read(&mut b, Waker::noop(), 1).is_pending());
        assert!(a.try_lock().is_none());
        gate.open();
        assert!(read(&mut b, Waker::noop(), 1).is_ready());
        let held = a.try_lock().expect("the stream");
        assert!(read(&mut b, Waker::noop(), 1).is_pending());
        drop(held);
        assert!(read(&mut b, Waker::noop(), 1).is_ready());
    }

    #[test]
    fn try_lock_fails_mid_operation_on_this_handle() {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut a = Shared::new(BufReader::new(Cursor::new(b"abcdef".to_vec())));
        assert!(Pin::new(&mut a).poll_fill_buf(&mut cx).is_ready());
        assert!(a.try_lock().is_none());
        Pin::new(&mut a).consume(1);
        assert!(a.try_lock().is_some());
    }

    #[test]
    fn try_lock_fails_once_closed() {
        let mut a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let b = a.clone();
        assert!(matches!(close(&mut a, Waker::noop()), Poll::Ready(Some(_))));
        assert!(a.try_lock().is_none());
        assert!(b.try_lock().is_none());
    }

    #[test]
    fn last_handle_unwraps() {
        let mut a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let b = a.clone();
        drop(b);
        assert!(a.is_shared());
        assert!(read(&mut a, Waker::noop(), 1).is_ready());
        assert!(!a.is_shared());
    }

    #[test]
    fn close_ends_every_handle() {
        let waker = Waker::noop();
        let mut a = Shared::new(Cursor::new(b"abcdef".to_vec()));
        let mut b = a.clone();
        assert!(matches!(close(&mut b, waker), Poll::Ready(Some(_))));
        assert!(matches!(close(&mut b, waker), Poll::Ready(None)));
        assert_closed(read(&mut a, waker, 1));
        assert_closed(read(&mut b, waker, 1));
        // Alone again, `a` unwraps and stays closed.
        drop(b);
        assert_closed(read(&mut a, waker, 1));
        assert!(!a.is_shared());
    }

    #[test]
    fn close_waits_for_an_operation() {
        let gate = Gate::default();
        let mut a = Shared::new(gate.clone());
        let mut b = a.clone();
        let (_, waker_a) = flag();
        let (woken_b, waker_b) = flag();
        assert!(read(&mut a, &waker_a, 1).is_pending());
        let mut closing = pin!(b.close());
        let mut cx = Context::from_waker(&waker_b);
        assert!(closing.as_mut().poll(&mut cx).is_pending());
        gate.open();
        assert!(read(&mut a, &waker_a, 1).is_ready());
        assert!(woken_b.0.load(Ordering::SeqCst));
        assert!(matches!(closing.poll(&mut cx), Poll::Ready(Some(_))));
        assert_closed(read(&mut a, &waker_a, 1));
    }

    #[test]
    fn poll_close_only_flushes() {
        let gate = Gate::default();
        let mut a = Shared::new(gate.clone());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut a).poll_close(&mut cx),
            Poll::Ready(Ok(()))
        ));
        let written = Pin::new(&mut a).poll_write(&mut cx, b"more");
        assert!(matches!(written, Poll::Ready(Ok(4))));
        let state = gate.0.lock().unwrap();
        assert_eq!((state.flushes, state.closes), (1, 0));
    }
}
