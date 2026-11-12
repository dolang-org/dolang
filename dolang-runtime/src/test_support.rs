//! Shared helpers for in-crate unit tests that need a live VM/`Strand`.

use crate::{
    arg::Args,
    strand::Strand,
    sym::Sym,
    value::{Slot, Slots},
    vm::Builder,
};

/// Build a VM and hand the body the [`Builder`] directly, before any `Strand` exists.
/// Blocks the calling thread. For tests that need `Builder`-only setup — registering a
/// symbol with [`Builder::sym`] (which stays permanently rooted for the life of the VM,
/// unlike a symbol interned via a `Strand` later) or a fixture type with
/// `Builder::register_type` — and then enter the VM themselves, typically via
/// `Builder::enter_with_slots`.
pub(crate) fn with_builder<R: 'static>(
    body: impl for<'v> AsyncFnOnce(&mut Builder<'v>) -> R + 'static,
) -> R {
    futures::executor::block_on(Builder::build(body))
}

/// Build a VM with default configuration, enter it, and run `body` with the resulting
/// `Strand` plus `N` GC-rooted scratch slots (see [`Strand::with_slots`]). Blocks the
/// calling thread. Values a test needs to hold onto — across a call that might allocate
/// or yield — belong in one of these slots, never in a bare, unrooted Rust local: use
/// `Slot::reborrow` to hand a slot to something that wants to fill it (e.g. as an `out`
/// parameter), and `Slot::into_ref`, or the slot itself as an `Input`, to read it back.
pub(crate) fn with_vm<const N: usize, R: 'static>(
    body: impl for<'v, 's, 'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R + 'static,
) -> R {
    with_builder(async move |vm| vm.enter_with_slots::<N, _>(body).await)
}

/// Builds `Args` over the backing storage of an already GC-rooted [`Slots`] value
/// (e.g. obtained from [`Strand::with_slots_dynamic`](crate::strand::Strand::with_slots_dynamic)).
/// Values must never be held in plain, unrooted Rust stack locals across a call that
/// could allocate or yield — this keeps the one `unsafe` `Args` construction it takes to
/// build a call-argument list contained here instead of scattered across test bodies.
///
/// Takes `slots` by exclusive reference, for the same lifetime as the returned `Args`,
/// so the borrow checker — not just a documented invariant — prevents the caller from
/// concurrently obtaining another `Slot` into the same backing storage (e.g. via
/// `Slots::at`) while `Args` is alive, which `Args::new`'s safety contract requires.
pub(crate) fn args_from_slots<'v, 'a>(
    slots: &'a mut Slots<'v, 'a>,
    sig: &'a [Option<Sym<'v, 'a>>],
    headroom: usize,
) -> Args<'v, 'a> {
    unsafe { Args::new(slots.as_inner(), sig, headroom) }
}
