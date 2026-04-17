use std::{fmt, ops::ControlFlow};

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    object::{
        iter,
        protocol::{GcObj, Protocol, Recv},
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
}

impl<'v> Protocol<'v> for Range<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().range)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().range.dup())
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        write!(w, "<range start: ").into_do(strand)?;
        borrow.start.op_debug(strand, &mut *w)?;
        if !borrow.end.is_nil() {
            write!(w, ", end: ").into_do(strand)?;
            borrow.end.op_debug(strand, &mut *w)?;
        }
        if !borrow.step.is_nil() {
            write!(w, ", step: ").into_do(strand)?;
            borrow.step.op_debug(strand, &mut *w)?;
        }
        write!(w, ">").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        let direction = if borrow.start.op_lt(strand, &borrow.end)?.to_bool(strand) {
            Direction::Increasing
        } else if borrow.start.op_gt(strand, &borrow.end)?.to_bool(strand) {
            Direction::Decreasing
        } else {
            Direction::Empty
        };
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().range_iter,
            Iter {
                cur: borrow.start.dup(),
                step: borrow.step.dup(),
                end: borrow.end.dup(),
                direction,
            },
        )));
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
            sym::CONTAINS => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.get();
                // Determine range direction and check containment accordingly
                // For increasing ranges (start < end): value >= start && value < end
                // For decreasing ranges (start > end): value > end && value <= start
                // For empty ranges (start == end): nothing is contained
                let is_increasing = borrow.start.op_lt(strand, &borrow.end)?.to_bool(strand);
                let is_decreasing = borrow.start.op_gt(strand, &borrow.end)?.to_bool(strand);
                let contained = if is_increasing {
                    // Increasing: [start, end)
                    let gte_start = !value.op_lt(strand, &borrow.start)?.to_bool(strand);
                    let lt_end = value.op_lt(strand, &borrow.end)?.to_bool(strand);
                    gte_start && lt_end
                } else if is_decreasing {
                    // Decreasing: (end, start]
                    let gt_end = borrow.end.op_lt(strand, &value)?.to_bool(strand);
                    let lte_start = !borrow.start.op_lt(strand, &value)?.to_bool(strand);
                    gt_end && lte_start
                } else {
                    // Empty range: start == end, nothing is contained
                    false
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
}

enum Direction {
    Empty,
    Increasing,
    Decreasing,
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<range iterator>").into_do(strand)
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
            Direction::Increasing => borrow.cur.op_lt(strand, &borrow.end)?.op_bool(strand),
            Direction::Decreasing => borrow.cur.op_gt(strand, &borrow.end)?.op_bool(strand),
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        use crate::error::ResultExt;
        write!(w, "<type std.range>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
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
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().range,
            Range::new(
                start
                    .as_mut()
                    .map(Slot::take)
                    .unwrap_or_else(|| Value::from_i64(strand, 0)),
                end,
                step.as_mut()
                    .map(Slot::take)
                    .unwrap_or_else(|| Value::from_i64(strand, 1)),
            ),
        )));
        Ok(())
    }
}
