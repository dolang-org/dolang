//! Iterator adapters that regroup or reshape their source: `flat_map`/`flatten`,
//! `take_while`/`skip_while`, `batches`, `tuples` and `windows`, plus the
//! `group_by` collector.

use std::{collections::VecDeque, hash::Hasher, ops::ControlFlow};

use super::{iter_get, iter_mcall, spread_iter};
use crate::{
    arg::Args,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::{
        array,
        protocol::{Protocol, Recv, Spread, SpreadContext},
        tuple,
    },
    strand::{Strand, with_slots_dynamic},
    sym::Sym,
    value::{Output, Slot, Value},
    vm::AllocExt,
    with_slots,
};

/// The members every adapter here shares: it's an `Iter` that iterates and
/// spreads as itself, with the `Iter` methods.
macro_rules! adapter_ops {
    ($v:lifetime) => {
        const MODULE: &$v str = "std";

        fn op_type<'a, 's>(
            _this: Recv<$v, 'a, Self>,
            strand: &'a mut Strand<$v, 's>,
            out: Slot<$v, 'a>,
        ) {
            strand.store(out, &strand.singletons().input_iter)
        }

        async fn op_iter<'a, 's>(
            this: Recv<$v, 'a, Self>,
            strand: &'a mut Strand<$v, 's>,
            out: Slot<$v, 'a>,
        ) -> Result<$v, 's, ()> {
            strand.store(out, &this);
            Ok(())
        }

        fn op_get<'a, 's>(
            this: Recv<$v, 'a, Self>,
            strand: &'a mut Strand<$v, 's>,
            field: Sym<$v, 'a>,
            out: Slot<$v, 'a>,
        ) -> Result<$v, 's, ()> {
            iter_get(strand, &this, field, out)
        }

        async fn op_mcall<'a, 's>(
            this: Recv<$v, 'a, Self>,
            strand: &'a mut Strand<$v, 's>,
            method: Sym<$v, 'a>,
            args: Args<$v, 'a>,
            out: Slot<$v, 'a>,
        ) -> Result<$v, 's, ()> {
            iter_mcall(strand, &this, method, args, out).await
        }

        async fn op_spread<'a, 's>(
            this: Recv<$v, 'a, Self>,
            strand: &'a mut Strand<$v, 's>,
            _context: SpreadContext,
            sink: &'a mut dyn Spread<$v, 's>,
        ) -> Result<$v, 's, ()> {
            spread_iter(strand, this.clone(), sink).await
        }
    };
}

/// `flat_map` and `flatten`: the items of each iterable the source yields,
/// mapped through `func` first if there is one.
pub(crate) struct FlatMap<'v> {
    func: Option<Value<'v>>,
    source: Value<'v>,
    /// The iterator over the current item, if there is one.
    inner: Option<Value<'v>>,
}

/// `take_while`: the source's items up to the first that fails `pred`.
pub(crate) struct TakeWhile<'v> {
    pred: Value<'v>,
    source: Value<'v>,
    done: bool,
}

/// `skip_while`: the source's items from the first that fails `pred` on.
pub(crate) struct SkipWhile<'v> {
    pred: Value<'v>,
    source: Value<'v>,
    skipping: bool,
}

/// `batches`: arrays of `n` items, the last of them possibly short.
pub(crate) struct Batches<'v> {
    source: Value<'v>,
    n: usize,
    done: bool,
}

/// `tuples`: tuples of exactly `n` items.
pub(crate) struct Tuples<'v> {
    source: Value<'v>,
    n: usize,
    done: bool,
}

/// `windows`: tuples of each `n` consecutive items.
pub(crate) struct Windows<'v> {
    source: Value<'v>,
    n: usize,
    /// The last window yielded, or the items read toward the first.
    items: VecDeque<Value<'v>>,
    done: bool,
}

unsafe impl<'v> Collect for FlatMap<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        if let Some(func) = &self.func {
            func.accept(visit)?;
        }
        self.source.accept(visit)?;
        if let Some(inner) = &self.inner {
            inner.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.func = None;
        self.source.clear();
        self.inner = None;
    }
}

unsafe impl<'v> Collect for TakeWhile<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.pred.accept(visit)?;
        self.source.accept(visit)
    }

    fn clear(&mut self) {
        self.pred.clear();
        self.source.clear();
        self.done = true;
    }
}

unsafe impl<'v> Collect for SkipWhile<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.pred.accept(visit)?;
        self.source.accept(visit)
    }

    fn clear(&mut self) {
        self.pred.clear();
        self.source.clear();
    }
}

unsafe impl<'v> Collect for Batches<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)
    }

    fn clear(&mut self) {
        self.source.clear();
        self.done = true;
    }
}

unsafe impl<'v> Collect for Tuples<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)
    }

    fn clear(&mut self) {
        self.source.clear();
        self.done = true;
    }
}

unsafe impl<'v> Collect for Windows<'v> {
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
        self.done = true;
    }
}

impl<'v> Protocol<'v> for FlatMap<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "FlatMap";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut source,
                             mut func,
                             mut inner,
                             mut item,
                             mut mapped| {
            let (mapping, mut current) = {
                let borrow = this.borrow(strand)?;
                strand.store(&mut source, &borrow.source);
                if let Some(value) = &borrow.func {
                    strand.store(&mut func, value);
                }
                if let Some(value) = &borrow.inner {
                    strand.store(&mut inner, value);
                }
                (borrow.func.is_some(), borrow.inner.is_some())
            };
            loop {
                if current {
                    if inner.next(strand, Slot::reborrow(&mut out)).await? {
                        return Ok(true);
                    }
                    this.borrow_mut(strand)?.inner = None;
                }
                if !source.next(strand, &mut item).await? {
                    return Ok(false);
                }
                if mapping {
                    call!(strand, &func, &item, &mut mapped)?;
                    mapped.iter(strand, &mut inner).await?;
                } else {
                    item.iter(strand, &mut inner).await?;
                }
                // SAFETY: the copy moves straight into the borrowed adapter
                this.borrow_mut(strand)?.inner = Some(unsafe { inner.dup() });
                current = true;
                strand.check_trap_gc()?;
            }
        })
    }
}

impl<'v> Protocol<'v> for TakeWhile<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "TakeWhile";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut source, mut pred, mut res| {
            {
                let borrow = this.borrow(strand)?;
                if borrow.done {
                    return Ok(false);
                }
                strand.store(&mut source, &borrow.source);
                strand.store(&mut pred, &borrow.pred);
            }
            if !source.next(strand, Slot::reborrow(&mut out)).await? {
                this.borrow_mut(strand)?.done = true;
                return Ok(false);
            }
            call!(strand, &pred, &out, &mut res)?;
            if res.to_bool(strand)? {
                return Ok(true);
            }
            this.borrow_mut(strand)?.done = true;
            strand.store(out, Value::NIL);
            Ok(false)
        })
    }
}

impl<'v> Protocol<'v> for SkipWhile<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "SkipWhile";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut source, mut pred, mut res| {
            let skipping = {
                let borrow = this.borrow(strand)?;
                strand.store(&mut source, &borrow.source);
                strand.store(&mut pred, &borrow.pred);
                borrow.skipping
            };
            if !skipping {
                return source.next(strand, out).await;
            }
            loop {
                if !source.next(strand, Slot::reborrow(&mut out)).await? {
                    return Ok(false);
                }
                call!(strand, &pred, &out, &mut res)?;
                if !res.to_bool(strand)? {
                    this.borrow_mut(strand)?.skipping = false;
                    return Ok(true);
                }
                strand.check_trap_gc()?;
            }
        })
    }
}

impl<'v> Protocol<'v> for Batches<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "Batches";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut source, mut batch, mut item| {
            let n = {
                let borrow = this.borrow(strand)?;
                if borrow.done {
                    return Ok(false);
                }
                strand.store(&mut source, &borrow.source);
                borrow.n
            };
            strand.new_array(&mut batch);
            let mut read = 0;
            while read < n {
                if !source.next(strand, &mut item).await? {
                    this.borrow_mut(strand)?.done = true;
                    break;
                }
                array::push(strand, &batch, Slot::reborrow(&mut item))?;
                read += 1;
                strand.check_trap_gc()?;
            }
            if read == 0 {
                return Ok(false);
            }
            strand.store(out, &mut batch);
            Ok(true)
        })
    }
}

impl<'v> Protocol<'v> for Tuples<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "Tuples";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let n = {
            let borrow = this.borrow(strand)?;
            if borrow.done {
                return Ok(false);
            }
            borrow.n
        };
        // The first slot roots the source, the rest the items
        with_slots_dynamic!(strand, n + 1, |slots| {
            let mut slots: Vec<_> = slots.into_iter().collect();
            let (source, items) = slots.split_at_mut(1);
            let source = &mut source[0];
            {
                let borrow = this.borrow(strand)?;
                strand.store(Slot::reborrow(source), &borrow.source);
            }
            for (read, item) in items.iter_mut().enumerate() {
                if !source.next(strand, Slot::reborrow(item)).await? {
                    this.borrow_mut(strand)?.done = true;
                    if read == 0 {
                        return Ok(false);
                    }
                    return Err(Error::value(
                        strand,
                        format!("tuples: the source ended {read} items into a tuple of {n}"),
                    ));
                }
                strand.check_trap_gc()?;
            }
            let embryo = tuple::embryo(strand, n);
            embryo.finish_moving(strand, slots.drain(1..), out);
            Ok(true)
        })
    }
}

impl<'v> Protocol<'v> for Windows<'v> {
    adapter_ops!('v);
    const NAME: &'v str = "Windows";

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let n = this.borrow(strand)?.n;
        // The first slot roots the source, the next the item read, and the rest
        // the window
        with_slots_dynamic!(strand, n + 2, |slots| {
            let mut slots: Vec<_> = slots.into_iter().collect();
            let (head, window) = slots.split_at_mut(2);
            let [source, item] = head else {
                unreachable!("two head slots")
            };
            let missing = {
                let borrow = this.borrow(strand)?;
                if borrow.done {
                    return Ok(false);
                }
                strand.store(Slot::reborrow(source), &borrow.source);
                // Before the first window, read all of it; after, one more item
                n - borrow.items.len().min(n - 1)
            };
            for _ in 0..missing {
                if !source.next(strand, Slot::reborrow(item)).await? {
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.done = true;
                    let items = std::mem::take(&mut borrow.items);
                    drop(borrow);
                    drop(items);
                    return Ok(false);
                }
                let mut borrow = this.borrow_mut(strand)?;
                let old = if borrow.items.len() == n {
                    borrow.items.pop_front()
                } else {
                    None
                };
                // SAFETY: the item moves straight out of its rooted slot
                borrow.items.push_back(unsafe { item.take(strand) });
                drop(borrow);
                drop(old);
                strand.check_trap_gc()?;
            }
            {
                let borrow = this.borrow(strand)?;
                for (slot, value) in window.iter_mut().zip(borrow.items.iter()) {
                    strand.store(slot, value);
                }
            }
            let embryo = tuple::embryo(strand, n);
            embryo.finish_moving(strand, slots.drain(2..), out);
            Ok(true)
        })
    }
}

pub(crate) fn create_flat_map<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    func: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().flat_map_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the values move straight into it
    let adapter = unsafe {
        FlatMap {
            func: func.map(|mut func| func.take(strand)),
            source: source.dup(),
            inner: None,
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_take_while<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().take_while_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the values move straight into it
    let adapter = unsafe {
        TakeWhile {
            pred: pred.take(strand),
            source: source.dup(),
            done: false,
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_skip_while<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().skip_while_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the values move straight into it
    let adapter = unsafe {
        SkipWhile {
            pred: pred.take(strand),
            source: source.dup(),
            skipping: true,
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_batches<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    n: usize,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().batches_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let source = unsafe { source.dup() };
    let adapter = Batches {
        source,
        n,
        done: false,
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_tuples<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    n: usize,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().tuples_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let source = unsafe { source.dup() };
    let adapter = Tuples {
        source,
        n,
        done: false,
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_windows<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    n: usize,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().windows_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let source = unsafe { source.dup() };
    let adapter = Windows {
        source,
        n,
        items: VecDeque::with_capacity(n),
        done: false,
    };
    embryo.finish(strand, adapter, out);
}

/// The count `batches`, `tuples` and `windows` take.
pub(crate) fn positive_count<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, usize> {
    let count = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, "expected Int"))?;
    if count <= 0 {
        return Err(Error::value(strand, "expected positive Int"));
    }
    usize::try_from(count).map_err(|_| Error::overflow(strand))
}

/// Collects `obj`'s items into a dict of arrays by `key` of each, keeping the
/// groups and each group's items in the order they first appear.
pub(crate) async fn iter_group_by<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut key: Slot<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut groups,
                         mut func,
                         mut item,
                         mut group_key,
                         mut group| {
        strand.store(&mut func, &mut key);
        strand.new_dict(&mut groups);
        while obj.next(strand, &mut item).await? {
            call!(strand, &func, &item, &mut group_key)?;
            let dict = groups
                .downcast_ref(strand.builtin_types().dict)
                .expect("a dict");
            let found = with_pin!(strand, dict, |dict| {
                let borrow = dict.borrow().ok_or_else(|| Error::concurrency(strand))?;
                match borrow.get(strand, &group_key, None)? {
                    Some(existing) => {
                        strand.store(&mut group, existing);
                        Ok::<_, Error<'v, 's>>(true)
                    }
                    None => Ok(false),
                }
            })?;
            if found {
                array::push(strand, &group, Slot::reborrow(&mut item))?;
            } else {
                strand.new_array(&mut group);
                array::push(strand, &group, Slot::reborrow(&mut item))?;
                let mut hasher = strand.vm().hasher();
                group_key.hash(strand, &mut hasher)?;
                let hash = hasher.finish();
                let dict = groups
                    .downcast_ref(strand.builtin_types().dict)
                    .expect("a dict");
                with_pin!(strand, dict, |dict| {
                    dict.borrow_mut()
                        .ok_or_else(|| Error::concurrency(strand))?
                        .insert(
                            strand,
                            Slot::reborrow(&mut group_key),
                            Slot::reborrow(&mut group),
                            hash,
                            false,
                        )
                })?;
            }
            strand.check_trap_gc()?;
        }
        strand.store(out, &mut groups);
        Ok(())
    })
}
