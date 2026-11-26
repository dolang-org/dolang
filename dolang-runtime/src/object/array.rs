use std::ops::ControlFlow;

use crate::value::fmt::{Format, Spec};

use dolang_util::pairsort;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::{Unpack, members},
    strand::{Strand, with_slots_dynamic},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
    vm::{AllocExt, Vm},
    with_slots,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        type_mcall_fallback,
    },
    range, tuple,
};

pub(crate) struct Iter<'v> {
    array: GcObj<'v, Array<'v>>,
    index: usize,
}

/// Appends spread values to an array object that is already allocated, so each value is
/// rooted as soon as it arrives.
struct ArraySpread<'a, 'v>(&'a Value<'v>);

impl<'v> ArraySpread<'_, 'v> {
    fn push<'s>(&self, strand: &mut Strand<'v, 's>, mut value: Slot<'v, '_>) -> Result<'v, 's, ()> {
        let array = self
            .0
            .downcast_ref(strand.builtin_types().array)
            .expect("array accumulator");
        with_pin!(strand, array, |array| {
            // SAFETY: the value moves straight into the borrowed array
            array
                .borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?
                .inner
                .push(unsafe { value.take(strand) });
            Ok(())
        })
    }
}

impl<'a, 'v, 's> Spread<'v, 's> for ArraySpread<'a, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.push(strand, value)
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unexpected_key(strand, key))
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unexpected_key(strand, key))
    }
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }

    fn clear(&mut self) {
        // array can't be cleared, but the array itself can be
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "ArrayIter";

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_pin!(strand, borrow.array, |array| {
            let array_borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let items = &array_borrow.inner;
            let start = borrow.index.min(items.len());
            unpack.sequence(
                strand,
                items.len() - start,
                |strand, i, slot| {
                    strand.store(slot, &items[start + i]);
                    Ok(())
                },
                |strand, count, slot| {
                    let embryo = strand.builtin_types().array_iter.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated iterator
                    embryo.finish(
                        strand,
                        Iter {
                            array: unsafe { borrow.array.dup() },
                            index: start + count,
                        },
                        slot,
                    );
                    Ok(())
                },
            )
        })
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let found = with_pin!(strand, borrow.array, |array| {
            let array_borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let value = array_borrow.inner.get(index);
            if let Some(value) = value {
                strand.store(out, value);
            }
            value.is_some()
        });
        borrow.index += usize::from(found);
        Ok(found)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

/// Rest of an array from `start`, which can be iterated, spread and unpacked
/// repeatedly.
pub(crate) struct Rest<'v> {
    array: GcObj<'v, Array<'v>>,
    start: usize,
}

unsafe impl<'v> crate::gc::ImmState for Rest<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Rest<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }
}

pub(crate) type RestObj<'v> = crate::gc::Imm<Rest<'v>>;

impl<'v> Protocol<'v> for RestObj<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "ArrayRest";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Iterable)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_mcall(strand, &this, method, args, out).await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let rest = this.get();
        let embryo = strand.builtin_types().array_iter.embryo(strand);
        // SAFETY: the reference moves straight into the allocated iterator
        let iter = Iter {
            array: unsafe { rest.array.dup() },
            index: rest.start,
        };
        embryo.finish(strand, iter, out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let rest = this.get();
        with_pin!(strand, rest.array, |array| {
            let borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let start = rest.start.min(borrow.inner.len());
            with_slots!(strand, |mut tmp| {
                for item in &borrow.inner[start..] {
                    strand.store(&mut tmp, item);
                    sink.positional(strand, Slot::reborrow(&mut tmp))?;
                }
                Ok(())
            })
        })
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let rest = this.get();
        with_pin!(strand, rest.array, |array| {
            let borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let items = &borrow.inner;
            let start = rest.start.min(items.len());
            unpack.sequence(
                strand,
                items.len() - start,
                |strand, i, slot| {
                    strand.store(slot, &items[start + i]);
                    Ok(())
                },
                |strand, count, slot| {
                    // SAFETY: `make_rest` calls it after allocating
                    make_rest(strand, || unsafe { rest.array.dup() }, start + count, slot);
                    Ok(())
                },
            )
        })
    }
}

/// Stores a rest of `array` from `start`.  `array` runs after the rest is
/// allocated, so the reference it returns moves straight into the rest.
fn make_rest<'v>(
    strand: &mut Strand<'v, '_>,
    array: impl FnOnce() -> GcObj<'v, Array<'v>>,
    start: usize,
    out: Slot<'v, '_>,
) {
    let embryo = strand.builtin_types().array_rest.embryo(strand);
    let array = array();
    embryo.finish_imm(strand, Rest { array, start }, out);
}

pub(crate) struct Sink<'v> {
    array: GcObj<'v, Array<'v>>,
}

unsafe impl<'v> crate::gc::ImmState for Sink<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Sink<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }
}

pub(crate) type SinkObj<'v> = crate::gc::Imm<Sink<'v>>;

impl<'v> Protocol<'v> for SinkObj<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "ArraySink";

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        with_pin!(strand, borrow.array, |array| {
            let mut array_borrow = array
                .borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?;
            // SAFETY: the value moves straight into the borrowed array
            array_borrow.inner.push(unsafe { value.take(strand) });
            Ok(())
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::sink_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::sink_mcall(strand, &this, method, args, out).await
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter)
    }
}

pub(crate) struct Pairs<'v> {
    array: GcObj<'v, Array<'v>>,
    index: usize,
}

unsafe impl<'v> Collect for Pairs<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }

    fn clear(&mut self) {
        // array can't be cleared, but the array itself can
    }
}

impl<'v> Protocol<'v> for Pairs<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "ArrayPairs";

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_pin!(strand, borrow.array, |array| {
            let array_borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let items = &array_borrow.inner;
            let start = borrow.index.min(items.len());
            unpack.sequence(
                strand,
                items.len() - start,
                |strand, i, slot| {
                    let index = i64::try_from(start + i).map_err(|_| Error::overflow(strand))?;
                    tuple::create_pair(strand, index, &items[start + i], slot);
                    Ok(())
                },
                |strand, count, slot| {
                    let embryo = strand.builtin_types().array_pairs.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated iterator
                    embryo.finish(
                        strand,
                        Pairs {
                            array: unsafe { borrow.array.dup() },
                            index: start + count,
                        },
                        slot,
                    );
                    Ok(())
                },
            )
        })
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let found = with_pin!(strand, borrow.array, |array| 'body: {
            let array_borrow = array.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let Some(value) = array_borrow.inner.get(index) else {
                break 'body false;
            };
            tuple::create_pair(strand, index as i64, value, out);
            true
        });
        borrow.index += usize::from(found);
        Ok(found)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

pub(crate) struct Array<'v> {
    pub(crate) inner: Vec<Value<'v>>,
}

unsafe impl<'v> Collect for Array<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for item in self.inner.iter() {
            item.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<'v> Array<'v> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    /// Store an array of `args`, which must all be positional, into `out`.
    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let args = args.positional(strand)?;
        let embryo = strand.builtin_types().array.embryo(strand);
        // SAFETY: the array is allocated; the values move from their rooted slots
        // straight into it
        let inner = args
            .map(|mut value| unsafe { value.take(strand) })
            .collect();
        embryo.finish(strand, Self { inner }, out);
        Ok(())
    }

    /// Store an array of `args` into `out`, expanding `...` spreads (keyed `ITER`).
    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let spreads = args
            .reborrow()
            .any(|arg| matches!(arg, Arg::Key(sym, _) if sym.tag() == sym::ITER));
        if !spreads {
            return Self::from_args(strand, args, out);
        }
        // Spreading runs arbitrary code, so the values go straight into the allocated array
        let mut array = Slot::from_output(&mut out);
        strand
            .builtin_types()
            .array
            .create(strand, Self::new(), Slot::reborrow(&mut array));
        let mut sink = ArraySpread(&array);
        let mut counter = 0;
        loop {
            counter += 1;
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?;
            }
            match args.next() {
                Some(Arg::Pos(item)) => sink.push(strand, item)?,
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    expand
                        .op_spread(strand, SpreadContext::Sequence, &mut sink)
                        .await?;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Ok(()),
            }
        }
    }

    async fn sort<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = Sym::well_known(sym::KEY);
        let reverse = Sym::well_known(sym::REVERSE);
        unpack!(strand, args => ?:key, ?:reverse);
        let reverse = reverse
            .map(|r| {
                r.as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "reverse: expected Bool"))
            })
            .transpose()?
            .unwrap_or(false);

        if let Some(key_fn) = key {
            let borrow = this.borrow(strand)?;
            let len = borrow.inner.len();
            with_slots_dynamic!(strand, len, |mut slots| {
                {
                    for (i, value) in borrow.inner.iter().enumerate() {
                        if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                            strand.check_trap_gc()?;
                        }
                        let out = slots.at(i);
                        call!(strand, &key_fn, value, out)?;
                    }
                }

                // The keys stay in their slots; their indices sort with the elements
                let keys: Vec<_> = slots.iter_mut().collect();
                let mut order: Vec<usize> = (0..len).collect();
                drop(borrow);
                let mut borrow = this.borrow_mut(strand)?;
                let mut compare_count = 0usize;
                pairsort::sort_by(&mut order, borrow.inner.as_mut_slice(), |&lhs, &rhs| {
                    compare_count += 1;
                    if compare_count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    let (lhs, rhs) = (&*keys[lhs], &*keys[rhs]);
                    if reverse {
                        rhs.lt(strand, lhs)
                    } else {
                        lhs.lt(strand, rhs)
                    }
                })
            })
        } else {
            let mut borrow = this.borrow_mut(strand)?;
            let mut payload = vec![(); borrow.inner.len()];
            let mut compare_count = 0usize;
            pairsort::sort_by(
                borrow.inner.as_mut_slice(),
                payload.as_mut_slice(),
                |lhs, rhs| {
                    compare_count += 1;
                    if compare_count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if reverse {
                        rhs.lt(strand, lhs)
                    } else {
                        lhs.lt(strand, rhs)
                    }
                },
            )
        }
    }
}

impl<'v> Protocol<'v> for Array<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Array";

    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        use crate::value::fmt::{Fill, Kind, Pad};

        let kind = spec
            .kind
            .ok_or_else(|| crate::value::fmt::unresolved_kind(strand))?;
        if !kind.is_text() || spec.sign.is_some() || spec.fill == Fill::Zero {
            return Err(Error::type_error(strand, "unsupported array format option"));
        }
        let mut pad = Pad::new(*spec, w);
        if !spec.alt {
            match kind {
                Kind::Str => Self::op_display(this, strand, &mut pad)?,
                Kind::Dbg => Self::op_debug(this, strand, &mut pad)?,
                Kind::Verbatim => Self::op_verbatim(this, strand, &mut pad)?,
                _ => unreachable!(),
            }
            return pad.finish(strand);
        }

        let borrow = this.borrow(strand)?;
        if borrow.inner.is_empty() {
            crate::fmt!(strand, &mut pad, "[]")?;
        } else {
            crate::fmt!(strand, &mut pad, "[\n")?;
            for (index, item) in borrow.inner.iter().enumerate() {
                if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let child_spec = Spec {
                    alt: item.downcast_ref(strand.builtin_types().array).is_some()
                        || item.downcast_ref(strand.builtin_types().dict).is_some(),
                    kind: Some(kind),
                    ..Default::default()
                };
                let mut child = String::new();
                item.fmt(strand, &child_spec, &mut child)?;
                let mut indented = String::new();
                crate::value::fmt::push_indented(&mut indented, &child, 2);
                crate::fmt!(strand, &mut pad, "{indented},\n")?;
            }
            crate::fmt!(strand, &mut pad, "]")?;
        }
        pad.finish(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let this_borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "[")?;
        let inner = &this_borrow.inner;
        let mut iter = inner.iter();
        if let Some(first) = iter.next() {
            first.op_debug(strand, w)?;
            for i in 1..inner.len() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap()?;
                }
                crate::fmt!(strand, w, ", ")?;
                unsafe { inner.get_unchecked(i) }.op_debug(strand, w)?;
            }
        }
        crate::fmt!(strand, w, "]")
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand).expect("conflicting borrow");
        Ok(!borrow.inner.is_empty())
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner = &borrow.inner;
        for i in 0..inner.len() {
            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap()?;
            }
            let elem = unsafe { inner.get_unchecked(i) };
            elem.op_hash(strand, hasher)?;
        }
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let other = if let Some(other) = other.downcast_native(strand, strand.builtin_types().array)
        {
            other
        } else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let this_borrow = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let left = &this_borrow.inner;
            let right = &other_borrow.inner;
            if left.len() != right.len() {
                strand.store(&mut out, Value::from_bool(false));
                return Ok(());
            }
            for i in 0..left.len() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap()?;
                }
                let l = unsafe { left.get_unchecked(i) };
                let r = unsafe { right.get_unchecked(i) };
                if !l.eq(strand, r)? {
                    strand.store(&mut out, Value::from_bool(false));
                    return Ok(());
                }
            }
            strand.store(&mut out, Value::from_bool(true));
            Ok(())
        })
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().array) else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let this_borrow = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let left = &this_borrow.inner;
            let right = &other_borrow.inner;
            for i in 0..left.len().min(right.len()) {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap()?;
                }
                let l = unsafe { left.get_unchecked(i) };
                let r = unsafe { right.get_unchecked(i) };
                if !l.eq(strand, r)? {
                    let flag = l.lt(strand, r)?;
                    strand.store(&mut out, flag);
                    return Ok(());
                }
            }
            strand.store(&mut out, Value::from_bool(right.len() > left.len()));
            Ok(())
        })
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        if let Some(slice) = range::slice(index, strand, borrow.inner.len())? {
            if let range::Slice::Contiguous { start, end } = slice
                && borrow.inner.get(start..end).is_none()
            {
                return Err(Error::index(strand));
            }
            let embryo = strand.builtin_types().array.embryo(strand);
            let mut array = Array::new();
            // SAFETY (both arms): the array is allocated; the copies move straight into
            // its payload
            match slice {
                range::Slice::Contiguous { start, end } => {
                    array.inner.extend(
                        borrow.inner[start..end]
                            .iter()
                            .map(|value| unsafe { value.dup() }),
                    );
                }
                range::Slice::Stepped(indices) => {
                    array.inner.extend(
                        indices
                            .into_iter()
                            .map(|i| unsafe { borrow.inner[i].dup() }),
                    );
                }
            }
            embryo.finish(strand, array, out);
            return Ok(());
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        let index =
            index::element(borrow.inner.len(), index).ok_or_else(|| Error::index(strand))?;
        match borrow.inner.get(index) {
            Some(value) => {
                strand.store(out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let len = this.borrow(strand)?.inner.len();
        if let Some(slice) = range::slice(&index, strand, len)? {
            let range::Slice::Contiguous { start, end } = slice else {
                return Err(Error::index(strand));
            };
            if let Some(source) = value.downcast_native(strand, strand.builtin_types().array) {
                return with_pin!(strand, source, |source| {
                    let source_borrow =
                        source.borrow().ok_or_else(|| Error::concurrency(strand))?;
                    if let Some(mut target_borrow) = this.receiver.borrow_mut() {
                        // SAFETY: the copies move straight into the target's payload
                        target_borrow.inner.splice(
                            start..end,
                            source_borrow
                                .inner
                                .iter()
                                .map(|value| unsafe { value.dup() }),
                        );
                        return Ok(());
                    }
                    // Assigning an array into itself: splice from a snapshot
                    with_slots!(strand, |mut snapshot| {
                        // SAFETY: the tuple is allocated before the iterator runs, so the
                        // copies move straight into it
                        tuple::create(
                            strand,
                            source_borrow
                                .inner
                                .iter()
                                .map(|value| unsafe { value.dup() }),
                            &mut snapshot,
                        );
                        drop(source_borrow);
                        let snapshot = snapshot
                            .downcast_ref(strand.builtin_types().tuple)
                            .expect("tuple");
                        with_pin!(strand, snapshot, |snapshot| {
                            // SAFETY: the copies move straight into the target's payload
                            this.borrow_mut(strand)?.inner.splice(
                                start..end,
                                snapshot.items().iter().map(|value| unsafe { value.dup() }),
                            );
                            Ok(())
                        })
                    })
                });
            }
            if let Some(source) = value.downcast_ref(strand.builtin_types().tuple) {
                return with_pin!(strand, source, |source| {
                    // SAFETY: the copies move straight into the target's payload
                    this.borrow_mut(strand)?.inner.splice(
                        start..end,
                        source.items().iter().map(|value| unsafe { value.dup() }),
                    );
                    Ok(())
                });
            }
            // Only a sequence with a settled length replaces a slice. Anything
            // else -- a range, an iterator, a lone value -- would have to be
            // drained to know what it is replacing the slice with, and a
            // replacement whose length is discovered mid-splice is a worse
            // outcome than saying so up front.
            return Err(Error::type_error(
                strand,
                "range assignment requires an array or tuple",
            ));
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        let mut borrow = this.borrow_mut(strand)?;
        let index =
            index::element(borrow.inner.len(), index).ok_or_else(|| Error::index(strand))?;
        match borrow.inner.get_mut(index) {
            Some(slot) => {
                // SAFETY: the value moves straight into the borrowed array
                *slot = unsafe { value.take(strand) };
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::PUSH => {
                let mut borrow = this.borrow_mut(strand)?;
                for arg in args {
                    match arg {
                        // SAFETY: the value moves straight into the borrowed array
                        Arg::Pos(mut slot) => borrow.inner.push(unsafe { slot.take(strand) }),
                        Arg::Key(key, _) => {
                            return Err(Error::unexpected_key(strand, key));
                        }
                    }
                }
                Ok(())
            }
            sym::INSERT => {
                let mut borrow = this.borrow_mut(strand)?;
                let index = match args.next() {
                    None => return Err(Error::missing_positional(strand, 0)),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => slot,
                };
                let mut value = match args.next() {
                    None => return Err(Error::missing_positional(strand, 1)),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => slot,
                };
                let i = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let i =
                    index::position(borrow.inner.len(), i).ok_or_else(|| Error::index(strand))?;
                if i > borrow.inner.len() {
                    return Err(Error::index(strand));
                }
                match args.next() {
                    // SAFETY: the value moves straight into the borrowed array
                    None => borrow.inner.insert(i, unsafe { value.take(strand) }),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => {
                        let mut values = vec![value, slot];
                        for arg in args {
                            match arg {
                                Arg::Pos(slot) => values.push(slot),
                                Arg::Key(key, _) => {
                                    return Err(Error::unexpected_key(strand, key));
                                }
                            }
                        }
                        borrow
                            .inner
                            // SAFETY: the values move straight into the borrowed array
                            .splice(
                                i..i,
                                values.into_iter().map(|mut v| unsafe { v.take(strand) }),
                            );
                    }
                }
                Ok(())
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => index, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let borrow = this.borrow(strand)?;
                match index::element(borrow.inner.len(), index)
                    .and_then(|index| borrow.inner.get(index))
                {
                    Some(value) => strand.store(&mut out, value),
                    None => {
                        if let Some(mut default) = default {
                            strand.store(&mut out, &mut default)
                        } else if let Some(or_else) = or_else {
                            call!(strand, or_else, out)?;
                        }
                    }
                }
                Ok(())
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => ?index, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let value = {
                    let mut borrow = this.borrow_mut(strand)?;
                    match index {
                        Some(index) => {
                            let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                            index::element(borrow.inner.len(), index)
                                .map(|index| borrow.inner.remove(index))
                        }
                        None => borrow.inner.pop(),
                    }
                };
                match value {
                    Some(value) => strand.store(&mut out, value),
                    None => {
                        if let Some(mut default) = default {
                            strand.store(&mut out, &mut default)
                        } else if let Some(or_else) = or_else {
                            call!(strand, or_else, out)?;
                        } else {
                            return Err(Error::index(strand));
                        }
                    }
                }
                Ok(())
            }
            sym::DELETE => {
                unpack!(strand, args => index);
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let mut borrow = this.borrow_mut(strand)?;
                let deleted = index::element(borrow.inner.len(), index).is_some();
                if let Some(index) = index::element(borrow.inner.len(), index) {
                    borrow.inner.remove(index);
                }
                strand.store(out, deleted);
                Ok(())
            }
            sym::CLEAR => {
                unpack!(strand, args =>);
                this.borrow_mut(strand)?.inner.clear();
                Ok(())
            }
            sym::COPY => {
                unpack!(strand, args =>);
                if this.delegator().is_some() {
                    return with_slots!(strand, |mut receiver, mut ty| {
                        strand.store(Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, &receiver, out)
                    });
                }
                let borrow = this.borrow(strand)?;
                let embryo = strand.builtin_types().array.embryo(strand);
                // SAFETY: the array is allocated; the copies move straight into its payload
                let array = Array {
                    inner: borrow
                        .inner
                        .iter()
                        .map(|value| unsafe { value.dup() })
                        .collect(),
                };
                embryo.finish(strand, array, out);
                Ok(())
            }
            sym::SORT => Self::sort(this, strand, args).await,
            sym::PAIRS => {
                unpack!(strand, args =>);
                let embryo = strand.builtin_types().array_pairs.embryo(strand);
                // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
                embryo.finish(
                    strand,
                    Pairs {
                        array: unsafe { this.to_strong() },
                        index: 0,
                    },
                    out,
                );
                Ok(())
            }
            sym::CONTAINS => {
                unpack!(strand, args => needle);
                let borrow = this.borrow(strand)?;
                let len = borrow.inner.len();
                let mut found = false;
                for i in 0..len {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if unsafe { borrow.inner.get_unchecked(i) }.eq(strand, &needle)? {
                        found = true;
                        break;
                    }
                }
                strand.store(out, found);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "Array.len is a field, not a method",
            )),
            _ => iter::iterable_sinkable_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let input = this.borrow(strand)?.inner.len();
                strand.store(out, input);
                Ok(())
            }
            sym::PUSH
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::CLEAR
            | sym::COPY
            | sym::SORT
            | sym::PAIRS
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iterable_sinkable_get(strand, &this, field, out),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let embryo = strand.builtin_types().array_iter.embryo(strand);
        // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
        embryo.finish(
            strand,
            Iter {
                array: unsafe { this.to_strong() },
                index: 0,
            },
            out,
        );
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_slots!(strand, |mut tmp| {
            for item in borrow.inner.iter() {
                strand.store(&mut tmp, item);
                sink.positional(strand, Slot::reborrow(&mut tmp))?;
            }
            Ok(())
        })
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let items = &borrow.inner;
        unpack.sequence(
            strand,
            items.len(),
            |strand, i, slot| {
                strand.store(slot, &items[i]);
                Ok(())
            },
            |strand, index, slot| {
                // SAFETY: `make_rest` calls this after allocating the rest, whose
                // payload the copy of the pinned receiver becomes at once
                make_rest(strand, || unsafe { this.to_strong() }, index, slot);
                Ok(())
            },
        )
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let embryo = strand.builtin_types().array_sink.embryo(strand);
        // SAFETY: the sink is allocated; the pinned receiver is copied straight into it
        let array = unsafe { this.to_strong() };
        embryo.finish_imm(strand, Sink { array }, out);
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().array)
    }
}

// ── Array Class ─────────────────────────────────────────────────

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
    const NAME: &'v str = "Array";
    const TYPE_OBJECT: bool = true;

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => items);
        // Iterating runs arbitrary code, so the values go straight into the allocated
        // array, which is borrowed only for each push
        strand
            .builtin_types()
            .array
            .create(strand, Array::new(), Slot::reborrow(&mut out));
        super::iter::spread_iter(strand, &items, &mut ArraySpread(&out)).await
    }

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
            || supertype
                .eq(strand, &strand.singletons().sinkable)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                ..Value,
                Method(sym::INIT_METHOD),
                Method(sym::SPREAD_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::LT_METHOD),
                Getter(sym::LEN),
                Method(sym::PUSH),
                Method(sym::INSERT),
                Method(sym::GET),
                Method(sym::POP),
                Method(sym::DELETE),
                Method(sym::CLEAR),
                Method(sym::COPY),
                Method(sym::SORT),
                Method(sym::PAIRS),
                Method(sym::CONTAINS),
                Method(sym::INDEX_METHOD),
                Method(sym::ASSIGN_METHOD),
                Method(sym::ITER_METHOD),
                Method(sym::UNPACK_METHOD),
            ],
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::PUSH
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::CLEAR
            | sym::COPY
            | sym::SORT
            | sym::PAIRS
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => self_val, items);
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().array, items, &mut native)?;
                    self_val.op_fill(
                        strand,
                        &strand.singletons().array,
                        Slot::reborrow(&mut native),
                    )?;
                    Ok(())
                })
            }
            _ => type_mcall_fallback(strand, &strand.singletons().array, method, args, out).await,
        }
    }
}
