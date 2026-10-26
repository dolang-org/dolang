use std::{hash::DefaultHasher, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        iter,
        protocol::{Protocol, Recv},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
};

pub(crate) struct Range<'v> {
    start: Value<'v>,
    end: Value<'v>,
    step: Value<'v>,
}

unsafe impl<'v> Collect for Range<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.start.accept(visit)?;
        self.end.accept(visit)?;
        self.step.accept(visit)
    }

    fn clear(&mut self) {
        self.start = Value::NIL;
        self.end = Value::NIL;
        self.step = Value::NIL;
    }
}

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
    range.get().slice_bounds(strand, len).map(Some)
}

pub(crate) fn slice<'v, 's>(
    index: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    len: usize,
) -> Result<'v, 's, Option<Slice>> {
    let Some(range) = index.downcast_ref(strand.builtin_types().range) else {
        return Ok(None);
    };
    range.get().slice(strand, len).map(Some)
}

impl<'v> Protocol<'v> for Range<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().range)
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
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().range) else {
            return Ok(Value::FALSE);
        };
        let left = this.get();
        let right = other.get();
        Ok(Value::from_bool(
            left.start.op_eq(strand, &right.start)?.to_bool(strand)?
                && left.end.op_eq(strand, &right.end)?.to_bool(strand)?
                && left.step.op_eq(strand, &right.step)?.to_bool(strand)?,
        ))
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
        strand.with_slots_sync(|strand, [mut start, mut step]| {
            Output::set(strand, &mut start, &borrow.start);
            if start.is_nil() {
                Output::set(strand, &mut start, 0);
            }
            let (_, direction) = direction(strand, &start, &borrow.end, borrow.step())?;
            Output::set(strand, &mut step, &borrow.step);
            if step.is_nil() {
                Output::set(strand, &mut step, 1);
            }
            strand.builtin_types().range_iter.create(
                strand,
                Iter {
                    cur: start.take(),
                    end: borrow.end.dup(),
                    step: step.take(),
                    direction,
                },
                out,
            );
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
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.get();
                let start = if borrow.start.is_nil() {
                    Value::from_i64(strand, 0)
                } else {
                    borrow.start.dup()
                };
                let (ascending, direction) = direction(strand, &start, &borrow.end, borrow.step())?;
                let in_bounds = match direction {
                    Direction::Empty => false,
                    Direction::Increasing => {
                        start.op_lte(strand, &value)?.to_bool(strand)?
                            && value.op_lt(strand, &borrow.end)?.to_bool(strand)?
                    }
                    Direction::Decreasing => {
                        borrow.end.op_lt(strand, &value)?.to_bool(strand)?
                            && value.op_lte(strand, &start)?.to_bool(strand)?
                    }
                    Direction::Unbounded => {
                        if ascending {
                            start.op_lte(strand, &value)?.to_bool(strand)?
                        } else {
                            value.op_lte(strand, &start)?.to_bool(strand)?
                        }
                    }
                };
                // Without a step, membership is the interval alone, so any
                // ordered type works. With one, only values a whole number of
                // steps from the start are members; `step - step` stands in for
                // zero in the step's type.
                let contained = match borrow.step() {
                    Some(step) if in_bounds => {
                        let zero = step.op_sub(strand, step)?;
                        value
                            .op_sub(strand, &start)?
                            .op_mod(strand, step)?
                            .op_eq(strand, &zero)?
                            .to_bool(strand)?
                    }
                    _ => in_bounds,
                };
                Output::set(strand, out, contained);
                Ok(())
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
                Output::set(strand, out, &this.get().start);
                Ok(())
            }
            sym::END => {
                Output::set(strand, out, &this.get().end);
                Ok(())
            }
            sym::STEP => {
                Output::set(strand, out, &this.get().step);
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
        Some(step) => {
            let next = start.op_add(strand, step)?;
            if next.op_gt(strand, start)?.to_bool(strand)? {
                true
            } else if next.op_lt(strand, start)?.to_bool(strand)? {
                false
            } else {
                return Err(Error::value(strand, "range step must not be zero"));
            }
        }
    };
    let direction = if end.is_nil() {
        Direction::Unbounded
    } else if ascending {
        if start.op_lt(strand, end)?.to_bool(strand)? {
            Direction::Increasing
        } else {
            Direction::Empty
        }
    } else if start.op_gt(strand, end)?.to_bool(strand)? {
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

pub(crate) struct Iter<'v> {
    cur: Value<'v>,
    step: Value<'v>,
    end: Value<'v>,
    direction: Direction,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.cur.accept(visit)?;
        self.end.accept(visit)?;
        self.step.accept(visit)
    }

    fn clear(&mut self) {
        self.cur = Value::NIL;
        self.end = Value::NIL;
        self.step = Value::NIL;
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<range iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let res = match borrow.direction {
            Direction::Empty => false,
            Direction::Increasing => borrow.cur.op_lt(strand, &borrow.end)?.op_bool(strand)?,
            Direction::Decreasing => borrow.cur.op_gt(strand, &borrow.end)?.op_bool(strand)?,
            Direction::Unbounded => true,
        };
        if res {
            Output::set(strand, out, &borrow.cur);
            borrow.cur = borrow.cur.op_add(strand, &borrow.step)?
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
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
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

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Range>")
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
        let ([], [mut pos1, mut pos2, mut pos3, mut start, mut end, mut step]) =
            unpack!(strand, args, 0, 3, start = None, end = None, step = None)?;
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
        let end = end
            .ok_or_else(|| Error::missing_key(strand, Sym::well_known(sym::END)))?
            .take();
        strand.builtin_types().range.create(
            strand,
            Range::new(
                start.as_mut().map(Slot::take).unwrap_or_else(|| Value::NIL),
                end,
                step.as_mut().map(Slot::take).unwrap_or_else(|| Value::NIL),
            ),
            out,
        );
        Ok(())
    }
}
