//! `Iter.buffer`: an iterator that can unpack any source by buffering it.
//!
//! Unpacking must not change the receiver, so a stream such as `map` can't
//! unpack. A buffer reads ahead as far as a pattern needs and keeps the items
//! it read, so a failed arm leaves them for the next one.
//!
//! The iterator `buffer` returns and every rest captured from it are cursors
//! into one shared [`BufferState`]. The state holds the items from the
//! least-advanced live cursor on. It tracks cursors weakly, so a rest that
//! becomes unreachable stops retaining items.

use std::{collections::VecDeque, ops::ControlFlow};

use dolang_bytecode::Rest;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::{GcObj, Protocol, Recv, Spread, SpreadContext, Unpack, WeakObj},
    strand::Strand,
    sym::Sym,
    value::{Output, Slot, Value, fmt::Format},
    vm::AllocExt,
    with_slots,
};

use super::iter::{iter_get, spread_iter, unpack_iter_mcall};

/// A cursor's entry in [`BufferState::cursors`].
struct Entry<'v> {
    cursor: WeakObj<'v, CursorObj<'v>>,
    /// Absolute index of the cursor's next item.
    pos: usize,
}

impl Entry<'_> {
    fn live(&self) -> bool {
        self.cursor.strong_count() != 0
    }
}

/// The state shared by a buffer's cursors.
pub(crate) struct BufferState<'v> {
    source: Value<'v>,
    /// Items from absolute index `base` on.
    items: VecDeque<Value<'v>>,
    base: usize,
    /// Whether `source` is exhausted.
    done: bool,
    /// Indexed by [`Cursor::id`]; `None` marks a free id.
    cursors: Vec<Option<Entry<'v>>>,
}

unsafe impl<'v> Collect for BufferState<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        for item in &self.items {
            item.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
        self.items.clear();
        self.cursors.clear();
        self.done = true;
    }
}

impl<'v> BufferState<'v> {
    /// Absolute index one past the last buffered item.
    fn end(&self) -> usize {
        self.base + self.items.len()
    }

    fn pos(&self, id: usize) -> usize {
        self.cursors[id]
            .as_ref()
            .expect("a live cursor has an entry")
            .pos
    }

    /// Takes an id for a new cursor in `cursors`.
    fn reserve(cursors: &mut Vec<Option<Entry<'v>>>) -> usize {
        match cursors.iter().position(Option::is_none) {
            Some(id) => id,
            None => {
                cursors.push(None);
                cursors.len() - 1
            }
        }
    }

    /// Records the cursor just created into `slot` with id `id` at `pos`.
    fn register(
        cursors: &mut [Option<Entry<'v>>],
        slot: &Value<'v>,
        handle: CursorHandle<'v>,
        id: usize,
        pos: usize,
    ) {
        let obj = slot
            .downcast_ref(handle)
            .expect("the cursor was just created");
        cursors[id] = Some(Entry {
            cursor: GcObj::downgrade(obj),
            pos,
        });
    }

    /// Moves cursor `id` one item forward and drops the items no live cursor can
    /// reach.
    fn advance(&mut self, id: usize) {
        if let Some(entry) = &mut self.cursors[id] {
            entry.pos += 1;
        }
        let mut least = usize::MAX;
        for entry in &mut self.cursors {
            match entry {
                Some(e) if e.live() => least = least.min(e.pos),
                Some(_) => *entry = None,
                None => {}
            }
        }
        let drop = least.min(self.end()).saturating_sub(self.base);
        self.items.drain(..drop);
        self.base += drop;
    }

    /// Appends `item` read from the source, or marks the source exhausted.
    ///
    /// # Safety
    ///
    /// `item` must be an owned reference moved straight out of a rooted location.
    unsafe fn push(&mut self, item: Option<Value<'v>>) {
        match item {
            Some(item) => self.items.push_back(item),
            None => self.done = true,
        }
    }
}

impl<'v> Protocol<'v> for BufferState<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<buffer state>")
    }
}

/// An iterator over a [`BufferState`].
pub(crate) struct Cursor<'v> {
    state: GcObj<'v, BufferState<'v>>,
    id: usize,
}

unsafe impl<'v> crate::gc::ImmState for Cursor<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Cursor<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.state.accept(visit)
    }
}

pub(crate) type CursorObj<'v> = crate::gc::Imm<Cursor<'v>>;

type CursorHandle<'v> = crate::object::protocol::TypeHandle<'v, CursorObj<'v>>;

/// Creates a buffer over `source` and stores its first cursor in `out`.
pub(crate) fn create<'v>(strand: &mut Strand<'v, '_>, source: &Value<'v>, out: impl Output<'v>) {
    with_slots!(strand, |mut state, mut cursor| {
        let embryo = strand.builtin_types().buffer_state.embryo(strand);
        // SAFETY: the copy moves straight into the allocated state
        embryo.finish(
            strand,
            BufferState {
                source: unsafe { source.dup() },
                items: VecDeque::new(),
                base: 0,
                done: false,
                cursors: Vec::new(),
            },
            &mut state,
        );
        let handle = strand.builtin_types().buffered_iter;
        let state_obj = state
            .downcast_ref(strand.builtin_types().buffer_state)
            .expect("the state was just created");
        let embryo = handle.embryo(strand);
        // SAFETY: the reference moves straight into the allocated cursor
        embryo.finish_imm(
            strand,
            Cursor {
                state: unsafe { state_obj.dup() },
                id: 0,
            },
            &mut cursor,
        );
        with_pin!(strand, *state_obj, |pin| {
            let mut borrow = pin.borrow_mut().expect("the state was just created");
            let id = BufferState::reserve(&mut borrow.cursors);
            BufferState::register(&mut borrow.cursors, &cursor, handle, id, 0);
        });
        strand.store(out, &mut cursor);
    })
}

/// Reads the next item from `this`'s source into the buffer, returning whether
/// there was one.
async fn fill<'v, 's>(
    this: &Recv<'v, '_, CursorObj<'v>>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, bool> {
    let state = &this.get().state;
    with_slots!(strand, |mut source, mut item| {
        with_pin!(strand, *state, |pin| {
            let borrow = pin.borrow().ok_or_else(|| Error::concurrency(strand))?;
            if borrow.done {
                return Ok(false);
            }
            strand.store(&mut source, &borrow.source);
        });
        let more = source.next(strand, &mut item).await?;
        with_pin!(strand, *state, |pin| {
            let mut borrow = pin.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
            // SAFETY: the item moves straight out of its rooted slot
            unsafe { borrow.push(more.then(|| item.take(strand))) };
        });
        Ok(more)
    })
}

impl<'v> Protocol<'v> for CursorObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Buffer>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let Cursor { state, id } = this.get();
        loop {
            let found = with_pin!(strand, *state, |pin| {
                let mut borrow = pin.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
                let pos = borrow.pos(*id);
                if pos < borrow.end() {
                    strand.store(&mut out, &borrow.items[pos - borrow.base]);
                    borrow.advance(*id);
                    Some(true)
                } else if borrow.done {
                    Some(false)
                } else {
                    None
                }
            });
            if let Some(found) = found {
                return Ok(found);
            }
            fill(&this, strand).await?;
        }
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let Cursor { state, id } = this.get();
        // Without a rest, one more item rules out extras.
        let need =
            unpack.required() + unpack.optional() + usize::from(unpack.pos_rest() == Rest::None);
        loop {
            let enough = with_pin!(strand, *state, |pin| {
                let borrow = pin.borrow().ok_or_else(|| Error::concurrency(strand))?;
                borrow.done || borrow.end() - borrow.pos(*id) >= need
            });
            if enough || !fill(&this, strand).await? {
                break;
            }
        }
        let handle = strand.builtin_types().buffered_iter;
        with_pin!(strand, *state, |pin| {
            let mut borrow = pin.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
            let pos = borrow.pos(*id);
            let available = borrow.end() - pos;
            let start = pos - borrow.base;
            let borrow = &mut *borrow;
            let BufferState { items, cursors, .. } = borrow;
            unpack.sequence(
                strand,
                available,
                |strand, i, slot| {
                    strand.store(slot, &items[start + i]);
                    Ok(())
                },
                |strand, count, mut slot| {
                    let rest = BufferState::reserve(cursors);
                    let embryo = handle.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated cursor
                    embryo.finish_imm(
                        strand,
                        Cursor {
                            state: unsafe { state.dup() },
                            id: rest,
                        },
                        Slot::reborrow(&mut slot),
                    );
                    BufferState::register(cursors, &slot, handle, rest, pos + count);
                    Ok(())
                },
            )
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack_iter_mcall(strand, &this, method, args, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
    }
}

#[cfg(test)]
mod tests {
    use dolang_bytecode::Variadic;

    use crate::{
        object::array::Array,
        sig,
        test_support::{with_cells, with_vm},
    };

    use super::*;

    /// The number of items `cursor`'s buffer holds.
    fn held<'v>(strand: &mut Strand<'v, '_>, cursor: &Value<'v>) -> usize {
        let handle = strand.builtin_types().buffered_iter;
        let obj = cursor.downcast_ref(handle).expect("a buffer cursor");
        with_pin!(strand, *obj, |pin| {
            let state = &pin.get().state;
            with_pin!(strand, *state, |state| state.borrow().unwrap().items.len())
        })
    }

    #[test]
    fn dropped_rest_releases_items() {
        with_vm(
            async |strand, [mut array, mut source, mut cursor, mut item]| {
                let embryo = strand.builtin_types().array.embryo(strand);
                let inner = (1..=4).map(Value::from_i16).collect();
                embryo.finish(strand, Array { inner }, Slot::reborrow(&mut array));
                array
                    .iter(strand, Slot::reborrow(&mut source))
                    .await
                    .unwrap();
                create(strand, &source, Slot::reborrow(&mut cursor));

                // `let _ *rest = cursor`: the rest starts after the first item.
                let sig = sig::Unpack::new(1, vec![], vec![], Variadic::Capture);
                with_cells!(strand, 2, |cells| {
                    // SAFETY: the cells are the frame's, rooted for the call, and no other
                    // slot into them is formed while the `Unpack` lives
                    cursor
                        .op_unpack(strand, unsafe { Unpack::new(&sig, cells) })
                        .await
                        .unwrap();
                    for _ in 0..3 {
                        assert!(
                            cursor
                                .next(strand, Slot::reborrow(&mut item))
                                .await
                                .unwrap()
                        );
                    }
                    // The rest still needs items 2 and 3.
                    assert_eq!(held(strand, &cursor), 2);
                    // SAFETY: the `Unpack` is gone, so this is the only slot into the cell
                    strand.store(unsafe { cells[1].slot() }, Value::NIL);
                    assert!(
                        cursor
                            .next(strand, Slot::reborrow(&mut item))
                            .await
                            .unwrap()
                    );
                    assert_eq!(held(strand, &cursor), 0);
                });
            },
        );
    }
}
