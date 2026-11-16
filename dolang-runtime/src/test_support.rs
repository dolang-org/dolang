//! Shared helpers for in-crate unit tests that need a live VM/`Strand`.

use crate::{strand::Strand, value::Slot, vm::Builder};

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
/// `Strand` plus `N` GC-rooted scratch slots (see [`with_slots!`](crate::with_slots)). Blocks the
/// calling thread. Values a test needs to hold onto — across a call that might allocate
/// or yield — belong in one of these slots, never in a bare, unrooted Rust local: use
/// `Slot::reborrow` to hand a slot to something that wants to fill it (e.g. as an `out`
/// parameter), and `Slot::into_ref`, or the slot itself as an `Input`, to read it back.
pub(crate) fn with_vm<const N: usize, R: 'static>(
    body: impl for<'v, 's, 'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R + 'static,
) -> R {
    with_builder(async move |vm| vm.enter_with_slots::<N, _>(body).await)
}

/// Run `body` with `len` nil cells rooted by a raw scan frame, bound as a
/// `&[ValueCell]` to the given name.
///
/// For tests that need contiguous cells, such as for `Args::new` or `Unpack::new`. As with
/// [`with_slots!`](crate::with_slots), `strand` is rebound to a handle whose frame roots
/// the cells. Writing a cell takes `unsafe { cells[i].slot() }`, and the test must form no
/// slot into a cell that an `Args` or `Unpack` over it can also write.
macro_rules! with_cells {
    ($strand:ident, $len:expr, |$cells:ident| $body:expr) => {{
        let cells: Box<[$crate::value::ValueCell<'_>]> =
            (0..$len).map(|_| Default::default()).collect();
        let visit = |visitor: &mut dyn $crate::gc::arena::Visit| {
            cells.iter().try_for_each(|cell| cell.accept(visitor))
        };
        let mut scope =
            $crate::strand::ScanScope::new($strand, &visit, |visit, visitor| visit(visitor));
        // SAFETY: the scope is a local of this block that nothing can move or forget, and
        // `visit` visits each cell once; cells change only through slots, between
        // collections
        #[allow(unused_variables)]
        let ($strand, _) = unsafe { scope.enter() };
        let $cells: &[$crate::value::ValueCell<'_>] = &cells;
        $body
    }};
}
pub(crate) use with_cells;
