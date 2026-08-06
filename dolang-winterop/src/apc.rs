//! A dedicated background thread that stays in an alertable wait, plus a
//! handle ([`Reactor`]) for running async work on it.
//!
//! Some Win32 APIs (most notably `NotifyServiceStatusChangeW`, used to
//! async-wait on SCM service status changes) deliver their completion as a
//! user-mode APC to whichever thread made the call, and require that thread
//! to periodically enter an alertable wait (`SleepEx`/`WaitForSingleObjectEx`
//! with `bAlertable = TRUE`) or the completion is never delivered. This
//! module is deliberately agnostic to any particular such API: it only
//! provides the alertable thread, task injection, cooperative cancellation,
//! and a raw-APC escape hatch that a specific binding (e.g. SCM) builds on.
//!
//! # Cancellation
//!
//! Dropping the [`ApcTask`] returned by [`Reactor::submit`] cancels the
//! task. By default this is a forced drop: the reactor removes the task
//! from its registry and drops its future in place, which is safe for
//! ordinary tasks. A task that needs to do something more careful before
//! being torn down (e.g. closing a handle that a still-in-flight completion
//! APC might reference) wraps the sensitive region in
//! [`ApcContext::cancel_guard`], which turns a cancellation request
//! arriving during that region into a cooperative `Err` instead of a forced
//! drop, so the task's own code can run `.await`-based cleanup before
//! finishing normally.

use std::{
    cell::RefCell,
    collections::HashMap,
    error, fmt, io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    ptr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    task::{Context, Poll, Waker},
    thread,
};

use futures::{
    channel::oneshot,
    future::{self, Either},
    task::ArcWake,
};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, TRUE},
    System::Threading::{GetCurrentProcess, GetCurrentThread, INFINITE, QueueUserAPC, SleepEx},
};

/// A future boxed for storage on the reactor thread. Deliberately not
/// `Send`: it is only ever constructed and polled on the reactor thread
/// itself (see [`Reactor::submit`]), never transported across a thread
/// boundary — which also sidesteps `AsyncFnOnce`'s associated future type
/// not being nameable as `Send` on stable Rust.
type BoxedTask = Pin<Box<dyn Future<Output = ()>>>;

/// Uniquely identifies a task within a single [`Reactor`]'s registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TaskId(u64);

/// Error returned by [`Reactor::submit`] when the reactor is no longer
/// accepting new work (after [`Reactor::cancel`], or if the reactor thread
/// has already exited).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("apc reactor is closed")
    }
}

impl error::Error for Closed {}

/// Error returned when awaiting an [`ApcTask`] whose task was cancelled
/// before producing a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskCancelled;

impl fmt::Display for TaskCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("apc task was cancelled")
    }
}

impl error::Error for TaskCancelled {}

/// Error returned by [`ApcContext::cancel_guard`] when a cancellation
/// request arrives while the guarded operation is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApcCancelled;

impl fmt::Display for ApcCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("apc task was cancelled inside a cancel_guard")
    }
}

impl error::Error for ApcCancelled {}

/// A per-task registry slot. Only ever touched by the reactor thread.
struct TaskSlot {
    /// Taken out for the duration of each poll so that reentrant access to
    /// this same slot's other fields (e.g. from `cancel_guard`, which runs
    /// as part of polling the task's own future) doesn't need a reentrant
    /// `RefCell` borrow.
    future: Option<BoxedTask>,
    in_guard: bool,
    guard_signal: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct Registry {
    tasks: HashMap<TaskId, TaskSlot>,
    /// Set by the reactor's own flush-marker APC (see `run`) to whatever
    /// `tasks`'s emptiness actually was at the moment that marker ran.
    /// This, not some value independently recomputed by the main loop, is
    /// the real exit condition: a task-insertion (or any other) APC can be
    /// durably queued without having run yet, in which case `tasks`
    /// doesn't reflect it yet and looks empty when it isn't really. The
    /// flush marker always runs strictly after anything queued before it
    /// (the OS's per-thread APC queue is FIFO), so its own check is
    /// authoritative for that instant.
    should_exit: bool,
}

thread_local! {
    /// Owned by the reactor thread's loop. Cross-thread requests
    /// (submission, cancellation) always arrive as a closure posted via a
    /// real `QueueUserAPC`, which only actually runs once executing on this
    /// thread — so nothing here needs locking.
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Queues `f` to run on the thread identified by `handle`, via a real
/// `QueueUserAPC`. This is the raw mechanism — no synchronization against
/// the reactor thread's own shutdown decision. Only [`ReactorInner::drop`],
/// [`ReactorControl::cancel`], and `run`'s own flush marker (see `run`) are
/// allowed to call this directly, since they can each prove nothing else
/// could be racing them; every other caller must go through [`post`], which
/// guards against a "successfully" queued APC being silently discarded when
/// the thread terminates before ever running it.
///
/// Takes a raw `HANDLE` rather than `&OwnedHandle` so `run` can pass the
/// pseudo-handle from `GetCurrentThread()` (valid only for the calling
/// thread to refer to itself, not an owned resource to be duplicated or
/// closed) when posting its own flush marker.
///
/// # Safety
///
/// `handle` must be a valid thread handle (with `THREAD_SET_CONTEXT`
/// access) for the entire duration of this call — either a real `HANDLE`
/// the caller keeps open (e.g. via a live `OwnedHandle` it's borrowing
/// from), or the `GetCurrentThread()` pseudo-handle used from within the
/// thread it refers to.
unsafe fn queue_apc(handle: HANDLE, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    unsafe extern "system" fn trampoline(param: usize) {
        // SAFETY: `param` was produced by `Box::into_raw` below, from a
        // `Box<Box<dyn FnOnce() + Send>>` that hasn't been freed yet (this
        // is the only place that ever reconstructs or frees it).
        let boxed = unsafe { Box::from_raw(param as *mut Box<dyn FnOnce() + Send>) };
        // Catch panics here: this runs across an `extern "system"`
        // boundary, where unwinding is undefined behavior. A panicking
        // closure (a bug in an injected task-insertion or cancel-dispatch
        // closure, say) shouldn't be able to bring down the whole process.
        let _ = catch_unwind(AssertUnwindSafe(move || (*boxed)()));
    }

    let boxed: Box<dyn FnOnce() + Send> = Box::new(f);
    let raw = Box::into_raw(Box::new(boxed));
    // SAFETY: `raw` is a valid, uniquely-owned pointer we just created;
    // `trampoline` reconstructs and consumes it exactly once, whenever the
    // OS actually delivers this APC.
    let ok = unsafe {
        QueueUserAPC(
            Some(trampoline as unsafe extern "system" fn(usize)),
            handle,
            raw as usize,
        )
    };
    if ok == 0 {
        // The APC will never run; reclaim the box instead of leaking it.
        drop(unsafe { Box::from_raw(raw) });
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Mutex-guarded state shared by every handle to a given reactor. Bundles
/// the thread handle together with `closed` so that touching the handle
/// for anything that needs to be synchronized with `closed` — which turns
/// out to be everything (see [`post`] and `run`) — is structurally forced
/// to go through the same lock, rather than relying on each call site to
/// separately remember to.
struct ReactorState {
    thread_handle: OwnedHandle,
    /// Set by [`ReactorControl::cancel`]. Once true, [`Reactor::submit`]
    /// rejects new work with [`Closed`].
    closed: bool,
}

/// State shared by every handle to a given reactor ([`Reactor`] clones,
/// [`ReactorControl`], and the [`ApcContext`]/[`ApcTask`] belonging to each
/// live task). Wrapped in a single `Arc` so there's one allocation and one
/// refcount for the whole reactor rather than three.
struct ReactorInner {
    state: Mutex<ReactorState>,
    next_id: AtomicU64,
}

impl Drop for ReactorInner {
    fn drop(&mut self) {
        // This only runs once every strong reference — every `Reactor`
        // clone, `ReactorControl`, and live task — is gone. [`post`] wraps
        // every closure it queues to hold its own `Arc<ReactorInner>` for
        // as long as it's queued-but-undelivered, so nothing can possibly
        // still be in flight at this point — a plain, unconditional wake is
        // enough to get the reactor thread to notice, via `Weak::upgrade`
        // failing in `run`, and exit. Unlike the explicit-`cancel` path, no
        // flush-and-recheck is needed here: nothing can still be racing us
        // (the only other way the reactor thread could be gone is this
        // very drop, which can't have run twice), and the handle is still
        // open at this point — only after this function returns does Rust
        // drop it (closing it) as an ordinary field.
        let guard = self.state.lock().unwrap();
        // SAFETY: `guard.thread_handle` is a live `OwnedHandle`, kept open
        // by holding `guard` (this field isn't dropped until this
        // function returns) for the duration of this call.
        let _ = unsafe { queue_apc(guard.thread_handle.as_raw_handle() as HANDLE, || {}) };
    }
}

/// Queues `f` to run on the reactor thread.
///
/// Doesn't need to check `closed` or otherwise synchronize with the reactor
/// thread's own shutdown decision, nor keep the reactor alive itself while
/// queued-but-undelivered: `run`'s loop never actually stops until its own
/// flush marker confirms the task registry is empty, and that marker is
/// always processed strictly after anything already durably queued at the
/// time it's posted (the OS's per-thread APC queue is FIFO) — so an APC
/// queued while the reactor thread is still willing to accept it is
/// guaranteed to run before the reactor exits, full stop.
fn post(inner: &Arc<ReactorInner>, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let guard = inner.state.lock().unwrap();
    // SAFETY: `guard.thread_handle` is a live `OwnedHandle`, kept open by
    // holding `guard` for the duration of this call.
    unsafe { queue_apc(guard.thread_handle.as_raw_handle() as HANDLE, f) }
}

/// Wakes the reactor thread's alertable wait so it re-polls its task set.
/// Shared by every task's [`Context`] — the reactor re-polls its whole
/// registry after every wake regardless of cause, so there is no need for
/// per-task wake identity.
///
/// Holds a `Weak` reference rather than a strong one: a waker can end up
/// cloned into and held by some external resource (e.g. a channel a task is
/// blocked on) for longer than the task itself, and a strong reference
/// there would keep the whole reactor alive even after every real handle to
/// it (`Reactor`, `ReactorControl`, the task itself) is gone.
struct WakeSignal {
    inner: Weak<ReactorInner>,
}

impl ArcWake for WakeSignal {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Best effort: failure, or the upgrade failing, means the reactor
        // thread has already exited (or is about to), so there is nothing
        // left to wake.
        if let Some(inner) = arc_self.inner.upgrade() {
            let _ = post(&inner, || {});
        }
    }
}

/// A cloneable handle for submitting work to a reactor. See the
/// [module docs](self) for the cancellation model.
///
/// Obtained alongside a [`ReactorControl`] from [`Reactor::new`] — that
/// separate, non-`Clone` type owns the reactor's lifecycle
/// (cancel/join), so that no arbitrary holder of a `Reactor` clone can
/// unilaterally shut it down for every other holder.
#[derive(Clone)]
pub struct Reactor {
    inner: Arc<ReactorInner>,
}

/// The lifecycle-owning counterpart to a [`Reactor`], returned once from
/// [`Reactor::new`]. Not `Clone` — cancellation and shutdown are meant to be
/// a single owner's responsibility, distinct from the freely-shared
/// submission capability held by [`Reactor`] clones.
pub struct ReactorControl {
    inner: Arc<ReactorInner>,
    exit_rx: oneshot::Receiver<()>,
}

impl Reactor {
    /// Spawns the reactor thread, returning a cloneable submission handle
    /// alongside the unique handle that controls its lifecycle.
    ///
    /// Async because it waits for the new thread to confirm it's alive
    /// before returning (see the comment in the spawned closure below for
    /// why) — that wait should always be brief, but it's still a wait for
    /// OS thread scheduling with no hard bound, so it shouldn't block
    /// whatever executor thread calls this.
    pub async fn new() -> io::Result<(Reactor, ReactorControl)> {
        let (exit_tx, exit_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let (handle_tx, handle_rx) = mpsc::channel::<Weak<ReactorInner>>();

        let join_handle = thread::Builder::new()
            .name("dolang-winterop-apc".into())
            .spawn(move || {
                // Signal that we are actually executing our own code before
                // doing anything else. A freshly created Windows thread can
                // still be inside the OS's own thread-startup sequence
                // (loader/CRT init) for a little while after `CreateThread`
                // returns a valid, already-usable handle; delivering a
                // `QueueUserAPC` to it during that window races that
                // startup and can corrupt it. Once *any* of our own code
                // has run, that window is guaranteed to be over, so the
                // spawning thread waits for this signal before it (or
                // anyone else) is allowed to post anything to us.
                let _ = ready_tx.send(());

                // Wait for a weak reference to the shared state, sent by
                // the spawning thread right after this thread was created
                // (see below — it can only be produced once the OS thread
                // exists, since it wraps this thread's own duplicated
                // handle). If the sender was dropped instead (spawning
                // failed after this thread was already created), just exit
                // without ever entering the alertable wait.
                let Ok(weak_inner) = handle_rx.recv() else {
                    return;
                };
                run(weak_inner);
                let _ = exit_tx.send(());
            })
            .map_err(io::Error::other)?;

        if ready_rx.await.is_err() {
            return Err(io::Error::other("apc reactor thread failed to start"));
        }

        // Duplicate a handle to the new thread that we own independent of
        // the `JoinHandle` — we never block-join the OS thread (`join()`
        // instead awaits `exit_rx`, signaled right before the thread's
        // closure returns), and detach it below.
        let mut dup: HANDLE = ptr::null_mut();
        // SAFETY: `join_handle.as_raw_handle()` is a valid, currently-open
        // thread handle for the thread we just spawned.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                join_handle.as_raw_handle() as HANDLE,
                GetCurrentProcess(),
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // Unblock the thread (it's parked on `handle_rx.recv()`) so it
            // exits immediately instead of waiting forever.
            drop(handle_tx);
            return Err(err);
        }
        // SAFETY: `dup` is a valid, uniquely-owned handle from a successful
        // `DuplicateHandle` call above.
        let thread_handle = unsafe { OwnedHandle::from_raw_handle(dup as _) };

        // Detach: dropping a `JoinHandle` without joining it just forfeits
        // the ability to block-join or observe a panic through it; the OS
        // thread keeps running independently, driven from here on by our
        // duplicated `thread_handle`.
        drop(join_handle);

        let inner = Arc::new(ReactorInner {
            state: Mutex::new(ReactorState {
                thread_handle,
                closed: false,
            }),
            next_id: AtomicU64::new(0),
        });

        // The receive end can only fail if the thread already exited
        // (e.g. it panicked before reaching `handle_rx.recv()`), in which
        // case there's nothing more to do — `exit_rx` will observe that on
        // its own once `ReactorControl` gets used.
        let _ = handle_tx.send(Arc::downgrade(&inner));

        Ok((
            Reactor {
                inner: inner.clone(),
            },
            ReactorControl { inner, exit_rx },
        ))
    }

    /// Submits `f` to run on the reactor thread, returning a future for its
    /// result.
    ///
    /// `f` receives an [`ApcContext`] for cooperative cancellation
    /// ([`ApcContext::cancel_guard`]) and raw APC posting
    /// ([`ApcContext::post_raw`]).
    ///
    /// Fails with [`Closed`] once [`cancel`](Self::cancel) has been called.
    pub fn submit<T, F>(&self, f: F) -> Result<ApcTask<T>, Closed>
    where
        T: Send + 'static,
        F: AsyncFnOnce(&mut ApcContext) -> T + Send + 'static,
    {
        // `closed` is read here, separately from the `post` call below, on
        // purpose: a submission that narrowly beats `cancel` (checks
        // `closed` just before it's set) isn't a bug — its task-insertion
        // APC simply shows up during the reactor's flush-and-recheck (see
        // `run`), which correctly aborts the exit rather than dropping it.
        if self.inner.state.lock().unwrap().closed {
            return Err(Closed);
        }

        let id = TaskId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (result_tx, result_rx) = oneshot::channel();

        let posted = post(&self.inner, move || {
            // Only construct (and box) the task's future once actually
            // running on the reactor thread — see `BoxedTask`'s doc comment
            // for why that matters.
            let task: BoxedTask = Box::pin(async move {
                let mut ctx = ApcContext { id };
                let value = f(&mut ctx).await;
                let _ = result_tx.send(value);
            });
            REGISTRY.with(|r| {
                r.borrow_mut().tasks.insert(
                    id,
                    TaskSlot {
                        future: Some(task),
                        in_guard: false,
                        guard_signal: None,
                    },
                );
            });
        });

        match posted {
            Ok(()) => Ok(ApcTask {
                id,
                rx: result_rx,
                inner: self.inner.clone(),
            }),
            Err(_) => Err(Closed),
        }
    }
}

impl ReactorControl {
    /// Stops accepting new [`Reactor::submit`] calls on every clone of the
    /// corresponding [`Reactor`].
    pub fn cancel(&self) {
        // `closed` is set and the wake is posted in the *same* critical
        // section — not two separate calls — because the reactor thread's
        // own "read `closed`, and if true, start exiting" step (`run`)
        // takes the same lock. Without that, the reactor could observe a
        // stale `closed == false` via some unrelated wake, decide to keep
        // looping, and go back to sleep with nothing left to ever wake it
        // again before this call's own post — permanently hanging a
        // reactor that had nothing else going on.
        let mut guard = self.inner.state.lock().unwrap();
        if guard.closed {
            return;
        }
        guard.closed = true;
        // Best effort: if the reactor already fully exited in the
        // meantime, this just fails harmlessly — there's nothing left to
        // wake.
        // SAFETY: `guard.thread_handle` is a live `OwnedHandle`, kept open
        // by holding `guard` for the duration of this call.
        let _ = unsafe { queue_apc(guard.thread_handle.as_raw_handle() as HANDLE, || {}) };
    }

    /// Awaits the reactor thread's actual exit.
    ///
    /// This does *not* call [`cancel`](Self::cancel) itself — it drops this
    /// handle's own reference to the reactor and then waits, so calling it
    /// without ever calling `cancel` waits for every `Reactor` clone and
    /// live task to also be dropped (releasing their own references) *and*
    /// for the reactor to quiesce, rather than forcing early shutdown while
    /// other handles might still be in use.
    ///
    /// Never resolves if a `Reactor` clone or live task is kept around
    /// forever without being dropped or cancelled.
    pub async fn join(self) {
        let ReactorControl { inner, exit_rx } = self;
        drop(inner);
        let _ = exit_rx.await;
    }
}

/// The main reactor loop: sleep alertably, re-poll every registered task,
/// repeat until [`Registry::should_exit`] says it's safe to stop.
fn run(weak_inner: Weak<ReactorInner>) {
    let waker = futures::task::waker(Arc::new(WakeSignal {
        inner: weak_inner.clone(),
    }));
    loop {
        // SAFETY: plain alertable wait; no preconditions beyond a valid
        // calling thread.
        unsafe {
            SleepEx(INFINITE, TRUE);
        }
        poll_all(&waker);

        if REGISTRY.with(|r| r.borrow().should_exit) {
            break;
        }

        // `closed` is either genuinely true (`ReactorControl::cancel` was
        // called), or *effectively* true because nobody could possibly
        // call it — or `Reactor::submit` — ever again: `weak_inner.upgrade`
        // failing means every `Reactor`, `ReactorControl`, and live task
        // reference is gone. Either way this alone doesn't mean it's safe
        // to stop: there could still be a live task in `registry`, or an
        // APC already durably queued but not yet reflected there. Keep
        // looping normally (the `if` below is only a cheap pre-filter, not
        // the real exit decision) until a flush marker actually confirms
        // it.
        let closed = match weak_inner.upgrade() {
            Some(inner) => inner.state.lock().unwrap().closed,
            None => true,
        };
        if closed && REGISTRY.with(|r| r.borrow().tasks.is_empty()) {
            // Posted via the `GetCurrentThread()` pseudo-handle, not
            // `ReactorInner`'s — which may already be gone in the
            // natural-quiescence case — since this must keep working
            // regardless. Its own check of `registry`, made at the moment
            // it actually runs (strictly after anything already durably
            // queued, per FIFO order — see `post`), is what's actually
            // authoritative; if something did sneak in, `should_exit`
            // simply comes out false and the loop above keeps running
            // normally until this is attempted again once things settle.
            // SAFETY: `GetCurrentThread()`'s pseudo-handle is always valid
            // for the thread it refers to, which is the one making this
            // call.
            let _ = unsafe {
                queue_apc(GetCurrentThread(), || {
                    REGISTRY.with(|r| {
                        let mut r = r.borrow_mut();
                        r.should_exit = r.tasks.is_empty();
                    });
                })
            };
        }
    }
}

fn poll_all(waker: &Waker) {
    let ids: Vec<TaskId> = REGISTRY.with(|r| r.borrow().tasks.keys().copied().collect());
    for id in ids {
        let future = REGISTRY.with(|r| {
            r.borrow_mut()
                .tasks
                .get_mut(&id)
                .and_then(|slot| slot.future.take())
        });
        let Some(mut future) = future else {
            // Not present (already retired) or its future was already
            // taken by an earlier iteration of this same pass — neither
            // can happen today since nothing re-enters `poll_all`
            // mid-pass, but skip defensively rather than panic.
            continue;
        };

        let mut cx = Context::from_waker(waker);
        let outcome = catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx)));

        match outcome {
            Ok(Poll::Pending) => {
                REGISTRY.with(|r| {
                    if let Some(slot) = r.borrow_mut().tasks.get_mut(&id) {
                        slot.future = Some(future);
                    }
                    // else: the slot was removed while its future was
                    // checked out above. Can't happen today (see above),
                    // but if it did, just let `future` drop here.
                });
            }
            Ok(Poll::Ready(())) | Err(_) => {
                // A panic while polling this task shouldn't take down the
                // shared reactor thread or strand other in-flight tasks —
                // just drop this one and move on.
                let removed = REGISTRY.with(|r| r.borrow_mut().tasks.remove(&id));
                drop(removed);
                drop(future);
            }
        }
    }
}

/// Capability object passed to a task submitted via [`Reactor::submit`],
/// providing cooperative cancellation and a raw-APC escape hatch.
///
/// Holds no reference to the reactor itself: every method here is only
/// ever called from within a task's own poll, which only ever happens on
/// the reactor thread, so [`post_raw`](Self::post_raw) can always reach it
/// directly via `GetCurrentThread()` instead.
pub struct ApcContext {
    id: TaskId,
}

impl ApcContext {
    /// Runs `operation`, allowing it to observe cancellation cooperatively
    /// instead of being force-dropped.
    ///
    /// While `operation` is running, a cancellation request for this task
    /// (i.e. its [`ApcTask`] being dropped) does not force-drop the task —
    /// instead this returns `Err(ApcCancelled)`, with `operation`'s
    /// in-progress future already dropped, so the surrounding task code can
    /// keep running `.await`-based cleanup (e.g. [`post_raw`](Self::post_raw)
    /// to safely drain a hazard window) before finishing.
    ///
    /// Only one `cancel_guard` may be active for a task at a time — calling
    /// this re-entrantly (before a previous guard's future has resolved)
    /// panics.
    pub async fn cancel_guard<T, F>(&mut self, operation: F) -> Result<T, ApcCancelled>
    where
        F: AsyncFnOnce(&mut ApcContext) -> T,
    {
        let id = self.id;
        let (tx, rx) = oneshot::channel::<()>();

        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            let slot = reg
                .tasks
                .get_mut(&id)
                .expect("cancel_guard: task slot missing for the currently running task");
            assert!(
                !slot.in_guard,
                "cancel_guard: already inside a guard for this task"
            );
            slot.in_guard = true;
            slot.guard_signal = Some(tx);
        });

        struct Reset(TaskId);
        impl Drop for Reset {
            fn drop(&mut self) {
                REGISTRY.with(|r| {
                    if let Some(slot) = r.borrow_mut().tasks.get_mut(&self.0) {
                        slot.in_guard = false;
                        slot.guard_signal = None;
                    }
                });
            }
        }
        let _reset = Reset(id);

        let operation_future = operation(&mut *self);
        futures::pin_mut!(operation_future);
        match future::select(operation_future, rx).await {
            Either::Left((value, _pending_cancel)) => Ok(value),
            Either::Right((_signal, _dropped_operation)) => Err(ApcCancelled),
        }
    }

    /// Posts a raw APC to the reactor thread directly, bypassing
    /// [`Reactor::submit`]'s closed-gate entirely — it only needs the
    /// reactor thread to still be alive, which it trivially is here since
    /// this is only ever called from within a task running on it.
    ///
    /// This is the primitive a hazard-sensitive consumer (e.g. SCM glue)
    /// uses to implement "close the handle, then prove no in-flight
    /// completion APC remains" before freeing memory a pending completion
    /// might still reference: because the OS's per-thread APC queue is
    /// FIFO regardless of who queues into it, posting (and awaiting) a raw
    /// APC immediately after closing the handle guarantees anything the
    /// kernel had already queued for that handle runs first.
    pub fn post_raw(&self, callback: impl FnOnce() + Send + 'static) -> io::Result<()> {
        // SAFETY: `ApcContext` methods only ever run from within a task's
        // poll, which only ever happens on the reactor thread — so
        // `GetCurrentThread()`'s pseudo-handle correctly refers to it.
        unsafe { queue_apc(GetCurrentThread(), callback) }
    }
}

/// A future for the result of a task submitted via [`Reactor::submit`].
///
/// Dropping this before it resolves cancels the task — see the
/// [module docs](self) for what that means for a task inside
/// [`ApcContext::cancel_guard`].
pub struct ApcTask<T> {
    id: TaskId,
    rx: oneshot::Receiver<T>,
    inner: Arc<ReactorInner>,
}

impl<T> Future for ApcTask<T> {
    type Output = Result<T, TaskCancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.rx)
            .poll(cx)
            .map(|result| result.map_err(|_| TaskCancelled))
    }
}

impl<T> Drop for ApcTask<T> {
    fn drop(&mut self) {
        let id = self.id;
        // Best effort: failure means the reactor thread (and thus the
        // task) has already gone away, so there's nothing to cancel.
        let _ = post(&self.inner, move || {
            let removed = REGISTRY.with(|r| {
                let mut reg = r.borrow_mut();
                match reg.tasks.get_mut(&id) {
                    None => None,
                    Some(slot) if slot.in_guard => {
                        if let Some(tx) = slot.guard_signal.take() {
                            let _ = tx.send(());
                        }
                        None
                    }
                    Some(_) => reg.tasks.remove(&id),
                }
            });
            // Drop outside the RefCell borrow above, in case the future's
            // own teardown happens to touch the registry.
            drop(removed);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    use super::*;

    /// Sends on `tx` when dropped — lets a test observe exactly when a
    /// task's future was actually torn down.
    struct DropSignal(Option<mpsc::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Joins `control` on a helper thread with a bounded wait, so a bug
    /// that makes `join()` hang doesn't hang the whole test suite.
    fn join_with_timeout(control: ReactorControl) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            futures::executor::block_on(control.join());
            let _ = tx.send(());
        });
        rx.recv_timeout(TIMEOUT)
            .expect("reactor did not shut down in time");
    }

    /// Runs `fut` to completion on a helper thread with a bounded wait, so a
    /// bug that stalls the reactor doesn't hang the whole test suite.
    fn block_on_with_timeout<T: Send + 'static>(
        fut: impl Future<Output = T> + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(futures::executor::block_on(fut));
        });
        rx.recv_timeout(TIMEOUT)
            .expect("future did not resolve in time")
    }

    #[test]
    fn submit_and_await_result() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();
        let task = reactor
            .submit(async move |_ctx: &mut ApcContext| 42)
            .unwrap();
        assert_eq!(block_on_with_timeout(task).unwrap(), 42);
        control.cancel();
        join_with_timeout(control);
    }

    #[test]
    fn dropping_unguarded_task_force_drops_it() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let task = reactor
            .submit(async move |_ctx: &mut ApcContext| {
                let _signal = DropSignal(Some(tx));
                let _ = started_tx.send(());
                future::pending::<()>().await
            })
            .unwrap();

        // Wait for the task to actually start running (and reach the
        // pending await) before cancelling it — otherwise we'd just be
        // testing that dropping a never-polled task drops it, which is a
        // trivially different (and trivially true) case.
        started_rx
            .recv_timeout(TIMEOUT)
            .expect("task should have started running");
        drop(task);

        rx.recv_timeout(TIMEOUT)
            .expect("unguarded task should be force-dropped promptly");
        control.cancel();
        join_with_timeout(control);
    }

    #[test]
    fn dropping_guarded_task_runs_cooperative_cleanup() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let task = reactor
            .submit(async move |ctx: &mut ApcContext| {
                let result = ctx
                    .cancel_guard(async move |_ctx: &mut ApcContext| {
                        let _ = started_tx.send(());
                        future::pending::<()>().await
                    })
                    .await;
                assert!(result.is_err(), "expected ApcCancelled");
                let _ = tx.send(());
            })
            .unwrap();

        // Wait for the task to actually enter its guard before cancelling
        // it, so this exercises the cooperative path rather than racing a
        // force-drop against the task's very first poll.
        started_rx
            .recv_timeout(TIMEOUT)
            .expect("task should have entered its guard");
        drop(task);

        rx.recv_timeout(TIMEOUT)
            .expect("guarded task should observe cancellation and clean up cooperatively");
        control.cancel();
        join_with_timeout(control);
    }

    #[test]
    fn cancel_rejects_new_submissions() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();
        control.cancel();

        let result = reactor.submit(async move |_ctx: &mut ApcContext| ());
        assert_eq!(result.err(), Some(Closed));

        join_with_timeout(control);
    }

    #[test]
    fn join_resolves_after_mixed_tasks_are_cancelled() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();

        let guarded = reactor
            .submit(async move |ctx: &mut ApcContext| {
                let _ = ctx
                    .cancel_guard(async move |_ctx: &mut ApcContext| future::pending::<()>().await)
                    .await;
            })
            .unwrap();
        let plain = reactor
            .submit(async move |_ctx: &mut ApcContext| future::pending::<()>().await)
            .unwrap();

        drop(guarded);
        drop(plain);
        control.cancel();

        // If this returns at all, `join()` correctly observed both
        // cancellations and drained the registry.
        join_with_timeout(control);
    }

    #[test]
    fn join_resolves_without_cancel_once_every_handle_is_dropped() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();

        let task = reactor
            .submit(async move |_ctx: &mut ApcContext| future::pending::<()>().await)
            .unwrap();

        // Drop every `Reactor` clone and live task, but never call
        // `cancel()`. `join()` should still resolve — it drops its own
        // reference and waits, so this exercises the reactor noticing that
        // *nothing* references it anymore (not just that it was told to
        // close) and exiting on its own.
        drop(task);
        drop(reactor);

        join_with_timeout(control);
    }

    #[test]
    fn post_raw_runs_on_reactor_thread() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        let task = reactor
            .submit(async move |ctx: &mut ApcContext| {
                let reactor_tid = unsafe { GetCurrentThreadId() };
                let (inner_tx, inner_rx) = oneshot::channel();
                ctx.post_raw(move || {
                    let _ = inner_tx.send(unsafe { GetCurrentThreadId() });
                })
                .unwrap();
                let posted_tid = inner_rx.await.unwrap();
                let _ = tx.send((reactor_tid, posted_tid));
            })
            .unwrap();

        let (reactor_tid, posted_tid) = rx.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(reactor_tid, posted_tid);

        drop(task);
        control.cancel();
        join_with_timeout(control);
    }

    #[test]
    fn panic_in_one_task_does_not_affect_others() {
        let (reactor, control) = futures::executor::block_on(Reactor::new()).unwrap();

        // Suppress the default panic hook's stderr output for this
        // deliberately-triggered, caught panic.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicking = reactor
            .submit(async move |_ctx: &mut ApcContext| {
                panic!("intentional test panic");
            })
            .unwrap();
        let panicking_result = block_on_with_timeout(panicking);
        std::panic::set_hook(previous_hook);
        assert!(panicking_result.is_err());

        let ok = reactor
            .submit(async move |_ctx: &mut ApcContext| 7)
            .unwrap();
        assert_eq!(block_on_with_timeout(ok).unwrap(), 7);

        control.cancel();
        join_with_timeout(control);
    }
}
