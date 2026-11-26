use std::{hash::DefaultHasher, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::{
        iter,
        protocol::{Protocol, Recv},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Slot, TypeObject, Value},
    vm::AllocExt,
    with_slots,
};

pub(crate) struct Range<'v> {
    start: Value<'v>,
    end: Value<'v>,
    step: Value<'v>,
}

unsafe impl<'v> crate::gc::ImmState for Range<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Range<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.start.accept(visit)?;
        self.end.accept(visit)?;
        self.step.accept(visit)
    }
}

pub(crate) type RangeObj<'v> = crate::gc::Imm<Range<'v>>;

impl<'v> Range<'v> {
    pub(crate) fn new(start: Value<'v>, end: Value<'v>, step: Value<'v>) -> Self {
        Self { start, end, step }
    }

    fn step(&self) -> Option<&Value<'v>> {
        (!self.step.is_nil()).then_some(&self.step)
    }

    pub(crate) fn parts(&self) -> (&Value<'v>, &Value<'v>, &Value<'v>) {
        (&self.start, &self.end, &self.step)
    }

    fn slice_bounds<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        len: usize,
    ) -> Result<'v, 's, (usize, usize)> {
        if !self.step.is_nil() && self.step.to_i64(strand).ok() != Some(1) {
            return Err(Error::index(strand));
        }
        let start = if self.start.is_nil() {
            0
        } else {
            crate::object::index::position(
                len,
                self.start
                    .to_i64(strand)
                    .map_err(|_| Error::index(strand))?,
            )
            .ok_or_else(|| Error::index(strand))?
        };
        let end = if self.end.is_nil() {
            len
        } else {
            crate::object::index::position(
                len,
                self.end.to_i64(strand).map_err(|_| Error::index(strand))?,
            )
            .ok_or_else(|| Error::index(strand))?
        };
        // An end before the start is an empty slice at the start.
        Ok((start, end.max(start)))
    }

    fn slice<'a, 's>(&self, strand: &'a mut Strand<'v, 's>, len: usize) -> Result<'v, 's, Slice> {
        let step = if self.step.is_nil() {
            1
        } else {
            self.step.to_i64(strand).map_err(|_| Error::index(strand))?
        };
        if step == 0 {
            return Err(Error::index(strand));
        }
        if step == 1 {
            let (start, end) = self.slice_bounds(strand, len)?;
            return Ok(Slice::Contiguous { start, end });
        }

        let len_i64 = i64::try_from(len).map_err(|_| Error::overflow(strand))?;
        let mut indices = Vec::new();
        if step > 0 {
            let start = if self.start.is_nil() {
                0
            } else {
                self.start
                    .to_i64(strand)
                    .map_err(|_| Error::index(strand))?
            };
            let end = if self.end.is_nil() {
                len_i64
            } else {
                self.end.to_i64(strand).map_err(|_| Error::index(strand))?
            };
            let start =
                crate::object::index::position(len, start).ok_or_else(|| Error::index(strand))?;
            let end =
                crate::object::index::position(len, end).ok_or_else(|| Error::index(strand))?;
            let mut i = i64::try_from(start).map_err(|_| Error::overflow(strand))?;
            let end = i64::try_from(end).map_err(|_| Error::overflow(strand))?;
            while i < end {
                indices.push(usize::try_from(i).map_err(|_| Error::overflow(strand))?);
                i = i.checked_add(step).ok_or_else(|| Error::overflow(strand))?;
            }
        } else {
            let start = if self.start.is_nil() {
                len_i64.checked_sub(1).unwrap_or(-1)
            } else {
                self.start
                    .to_i64(strand)
                    .map_err(|_| Error::index(strand))?
            };
            let end = if self.end.is_nil() {
                -1
            } else {
                self.end.to_i64(strand).map_err(|_| Error::index(strand))?
            };
            let start = reverse_index(len_i64, start).ok_or_else(|| Error::index(strand))?;
            let end = reverse_bound(len_i64, end).ok_or_else(|| Error::index(strand))?;
            let mut i = start;
            while i > end {
                indices.push(usize::try_from(i).map_err(|_| Error::overflow(strand))?);
                i = i.checked_add(step).ok_or_else(|| Error::overflow(strand))?;
            }
        }
        Ok(Slice::Stepped(indices))
    }
}

fn reverse_index(len: i64, index: i64) -> Option<i64> {
    let index = if index < 0 {
        len.checked_add(index)?
    } else {
        index
    };
    (0..len).contains(&index).then_some(index)
}

fn reverse_bound(len: i64, index: i64) -> Option<i64> {
    if index == -1 {
        return Some(-1);
    }
    let index = if index < 0 {
        len.checked_add(index)?
    } else {
        index
    };
    (0..len).contains(&index).then_some(index)
}

pub(crate) enum Slice {
    Contiguous { start: usize, end: usize },
    Stepped(Vec<usize>),
}

pub(crate) fn slice_bounds<'v, 's>(
    index: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    len: usize,
) -> Result<'v, 's, Option<(usize, usize)>> {
    let Some(range) = index.downcast_ref(strand.builtin_types().range) else {
        return Ok(None);
    };
    with_pin!(strand, range, |range| {
        range.get().slice_bounds(strand, len).map(Some)
    })
}

/// Resolves an optional `range:` window over a sequence of length `len`.
///
/// An omitted range is the whole sequence; otherwise it is bounded like a
/// slice, as with [`slice_bounds`].
pub(crate) fn window<'v, 's>(
    strand: &mut Strand<'v, 's>,
    range: Option<&Value<'v>>,
    len: usize,
) -> Result<'v, 's, (usize, usize)> {
    let Some(range) = range else {
        return Ok((0, len));
    };
    slice_bounds(range, strand, len)?
        .ok_or_else(|| Error::type_error(strand, "range: expected `Range`"))
}

pub(crate) fn slice<'v, 's>(
    index: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    len: usize,
) -> Result<'v, 's, Option<Slice>> {
    let Some(range) = index.downcast_ref(strand.builtin_types().range) else {
        return Ok(None);
    };
    with_pin!(strand, range, |range| {
        range.get().slice(strand, len).map(Some)
    })
}

impl<'v> Protocol<'v> for RangeObj<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Range";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().range)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        crate::fmt!(strand, w, "<Range start: ")?;
        borrow.start.op_debug(strand, &mut *w)?;
        if !borrow.end.is_nil() {
            crate::fmt!(strand, w, ", end: ")?;
            borrow.end.op_debug(strand, &mut *w)?;
        }
        if !borrow.step.is_nil() {
            crate::fmt!(strand, w, ", step: ")?;
            borrow.step.op_debug(strand, &mut *w)?;
        }
        crate::fmt!(strand, w, ">")
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().range) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.get();
        with_pin!(strand, other, |right| {
            let right = right.get();
            let flag = left.start.eq(strand, &right.start)?
                && left.end.eq(strand, &right.end)?
                && left.step.eq(strand, &right.step)?;
            strand.store(&mut out, flag);
            Ok(())
        })
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let this = this.get();
        this.start.op_hash(strand, hasher)?;
        this.end.op_hash(strand, hasher)?;
        this.step.op_hash(strand, hasher)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        with_slots!(strand, |mut start, mut step| {
            strand.store(&mut start, &borrow.start);
            if start.is_nil() {
                strand.store(&mut start, 0);
            }
            let (_, direction) = direction(strand, &start, &borrow.end, borrow.step())?;
            strand.store(&mut step, &borrow.step);
            if step.is_nil() {
                strand.store(&mut step, 1);
            }
            let embryo = strand.builtin_types().range_iter.embryo(strand);
            // SAFETY: the iterator is allocated; the bounds move from their rooted slots
            // straight into it
            let (iter, annex) = unsafe {
                (
                    Iter {
                        cur: start.take(strand),
                    },
                    IterAnnex {
                        end: borrow.end.dup(),
                        step: step.take(strand),
                        direction,
                    },
                )
            };
            embryo.finish_with_annex(strand, iter, annex, out);
            Ok(())
        })
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CONTAINS => {
                unpack!(strand, args => value);
                let borrow = this.get();
                with_slots!(strand, |mut start| {
                    if borrow.start.is_nil() {
                        strand.store(Slot::reborrow(&mut start), 0_i64);
                    } else {
                        strand.store(Slot::reborrow(&mut start), &borrow.start);
                    }
                    let (ascending, direction) =
                        direction(strand, &start, &borrow.end, borrow.step())?;
                    let in_bounds = match direction {
                        Direction::Empty => false,
                        Direction::Increasing => {
                            start.lte(strand, &value)? && value.lt(strand, &borrow.end)?
                        }
                        Direction::Decreasing => {
                            borrow.end.lt(strand, &value)? && value.lte(strand, &start)?
                        }
                        Direction::Unbounded => {
                            if ascending {
                                start.lte(strand, &value)?
                            } else {
                                value.lte(strand, &start)?
                            }
                        }
                    };
                    // Without a step, membership is the interval alone, so any
                    // ordered type works. With one, only values a whole number of
                    // steps from the start are members; `step - step` stands in for
                    // zero in the step's type.
                    let contained = match borrow.step() {
                        Some(step) if in_bounds => {
                            with_slots!(strand, |mut zero, mut offset, mut rem| {
                                step.op_sub(strand, step, Slot::reborrow(&mut zero))?;
                                value.op_sub(strand, &start, Slot::reborrow(&mut offset))?;
                                offset.op_mod(strand, step, Slot::reborrow(&mut rem))?;
                                rem.eq(strand, &zero)
                            })?
                        }
                        _ => in_bounds,
                    };
                    strand.store(out, contained);
                    Ok(())
                })
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::CONTAINS => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::START => {
                strand.store(out, &this.get().start);
                Ok(())
            }
            sym::END => {
                strand.store(out, &this.get().end);
                Ok(())
            }
            sym::STEP => {
                strand.store(out, &this.get().step);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
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

/// Picks the direction of travel from the step's sign: ascending when there is
/// no step, otherwise judged by whether `start + step` lands above or below
/// `start`, so any type with `+` and ordering works. Bounds that lie against
/// the step make the range empty; only a missing end is unbounded.
fn direction<'v, 's>(
    strand: &mut Strand<'v, 's>,
    start: &Value<'v>,
    end: &Value<'v>,
    step: Option<&Value<'v>>,
) -> Result<'v, 's, (bool, Direction)> {
    let ascending = match step {
        None => true,
        Some(step) => with_slots!(strand, |mut next| {
            start.op_add(strand, step, Slot::reborrow(&mut next))?;
            if next.gt(strand, start)? {
                Ok(true)
            } else if next.lt(strand, start)? {
                Ok(false)
            } else {
                Err(Error::value(strand, "range step must not be zero"))
            }
        })?,
    };
    let direction = if end.is_nil() {
        Direction::Unbounded
    } else if ascending {
        if start.lt(strand, end)? {
            Direction::Increasing
        } else {
            Direction::Empty
        }
    } else if start.gt(strand, end)? {
        Direction::Decreasing
    } else {
        Direction::Empty
    };
    Ok((ascending, direction))
}

enum Direction {
    Empty,
    Increasing,
    Decreasing,
    Unbounded,
}

/// A range iterator's mutable state: only the next value to yield.
pub(crate) struct Iter<'v> {
    cur: Value<'v>,
}

/// The parts of a range iterator fixed at construction.
pub(crate) struct IterAnnex<'v> {
    end: Value<'v>,
    step: Value<'v>,
    direction: Direction,
}

impl<'v> crate::gc::Annex for IterAnnex<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.end.accept(visit)?;
        self.step.accept(visit)
    }
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = IterAnnex<'v>;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.cur.accept(visit)
    }

    fn clear(&mut self) {
        self.cur = Value::NIL;
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "RangeIter";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let annex = this.annex();
        let mut borrow = this.borrow_mut(strand)?;
        let res = match annex.direction {
            Direction::Empty => false,
            Direction::Increasing => borrow.cur.lt(strand, &annex.end)?,
            Direction::Decreasing => borrow.cur.gt(strand, &annex.end)?,
            Direction::Unbounded => true,
        };
        if res {
            strand.store(out, &borrow.cur);
            with_slots!(strand, |mut next| {
                borrow
                    .cur
                    .op_add(strand, &annex.step, Slot::reborrow(&mut next))?;
                // SAFETY: the value moves straight into the borrowed iterator
                borrow.cur = unsafe { next.take(strand) };
                Ok(())
            })?;
        }
        Ok(res)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
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

// ── Range Class ─────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Range";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().iterable)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let start = Sym::well_known(sym::START);
        let end = Sym::well_known(sym::END);
        let step = Sym::well_known(sym::STEP);
        unpack!(strand, args =>
            ?mut pos1,
            ?mut pos2,
            ?mut pos3,
            ?start: mut start,
            ?end: mut end,
            ?step: mut step
        );
        let pos_count = pos1.is_some() as usize + pos2.is_some() as usize + pos3.is_some() as usize;
        end = end.or_else(|| {
            if pos_count > 1 {
                pos2.take()
            } else {
                pos1.take()
            }
        });
        start = start.or_else(|| pos1.take());
        step = step.or_else(|| pos3.take());
        if pos3.is_some() {
            return Err(Error::unexpected_positional(strand, 2));
        } else if pos2.is_some() {
            return Err(Error::unexpected_positional(strand, 1));
        } else if pos1.is_some() {
            return Err(Error::unexpected_positional(strand, 0));
        }
        let mut end = end.ok_or_else(|| Error::missing_key(strand, Sym::well_known(sym::END)))?;
        let embryo = strand.builtin_types().range.embryo(strand);
        // SAFETY: the range is allocated; the values move from their rooted slots
        // straight into it
        let range = unsafe {
            Range::new(
                start.map_or(Value::NIL, |mut start| start.take(strand)),
                end.take(strand),
                step.map_or(Value::NIL, |mut step| step.take(strand)),
            )
        };
        embryo.finish_imm(strand, range, out);
        Ok(())
    }
}
