use std::{
    cell::{Cell, RefCell},
    ops::ControlFlow,
};

use crate::value::fmt::{Format, Spec};

use bitvec::bitbox;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, iter,
    kv::{self, Inner, UnpackState},
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_mcall_fallback,
        is_special_mcall, type_mcall_fallback,
    },
};

// ── Dict newtype ────────────────────────────────────────────────────

struct DictPairs<'b, 'v> {
    int: i64,
    dict: &'b mut Dict<'v>,
}

impl<'b, 'v, 's> Spread<'v, 's> for DictPairs<'b, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_i64(strand, self.int);
        let hv = kv::hash(strand, &key).unwrap();
        self.dict.0.insert(strand, key, value.take(), hv, false);
        self.int = self
            .int
            .checked_add(1)
            .ok_or_else(|| Error::overflow(strand))?;
        Ok(())
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_object(strand.sym_obj(key));
        let hv = kv::hash(strand, &key).unwrap();
        self.dict.0.insert(strand, key, value.take(), hv, false);
        Ok(())
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let hv = kv::hash(strand, &key)?;
        self.dict
            .0
            .insert(strand, key.take(), value.take(), hv, false);
        Ok(())
    }
}

pub(crate) struct Dict<'v>(pub(crate) Inner<'v>);

impl<'v> AsRef<Inner<'v>> for Dict<'v> {
    fn as_ref(&self) -> &Inner<'v> {
        &self.0
    }
}

impl<'v> AsMut<Inner<'v>> for Dict<'v> {
    fn as_mut(&mut self) -> &mut Inner<'v> {
        &mut self.0
    }
}

unsafe impl<'v> Collect for Dict<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.0.accept(visit)
    }

    fn clear(&mut self) {
        self.0.clear()
    }
}

impl<'v> Dict<'v> {
    pub(crate) fn new() -> Self {
        Self(Inner::new())
    }

    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut sink = DictPairs {
            int: 0,
            dict: &mut this,
        };

        for (index, arg) in args.enumerate() {
            if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?;
            }
            match arg {
                Arg::Pos(value) => sink.positional(strand, value)?,
                Arg::Key(key, value) => sink.symbol(strand, key, value)?,
            }
        }

        Ok(this)
    }

    pub(crate) fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: &Value<'v>,
        instance: Option<i64>,
    ) -> Result<'v, 's, Option<&Value<'v>>> {
        self.0.get(strand, index, instance)
    }

    pub(crate) fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Value<'v>,
        value: Value<'v>,
        hv: u64,
        unique: bool,
    ) {
        self.0.insert(strand, key, value, hv, unique)
    }

    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut counter = 1;
        let mut index = 0;

        let mut sink = DictPairs {
            int: 0,
            dict: &mut this,
        };

        loop {
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?
            }
            counter += 1;
            let mut key = match args.next() {
                Some(Arg::Pos(key)) => key,
                Some(Arg::Key(sym, mut value)) if sym.tag() == sym::INT => {
                    let key = Value::from_i64(strand, index);
                    let hv = kv::hash(strand, &key).unwrap();
                    sink.dict.insert(
                        strand,
                        Value::from_i64(strand, index),
                        value.take(),
                        hv,
                        false,
                    );
                    index += 1;
                    continue;
                }
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    expand
                        .op_spread(strand, SpreadContext::Pairs, &mut sink)
                        .await?;
                    continue;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => break,
            };
            let mut value = match args.next() {
                Some(Arg::Pos(value)) => value,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Err(Error::missing_positional(strand, counter)),
            };
            let hv = kv::hash(strand, &key)?;
            sink.dict
                .insert(strand, key.take(), value.take(), hv, false)
        }

        Ok(this)
    }
}

// ── Iter ────────────────────────────────────────────────────────────

pub(crate) struct Iter<'v> {
    index: Cell<usize>,
    epoch: u64,
    dict: GcObj<'v, Dict<'v>>,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.dict.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dict iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _sig: &'a sig::Unpack<'v, 'a>,
        _out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::not_supported(strand))
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        kv::Inner::iter_op_next(&borrow.index, borrow.epoch, &borrow.dict, strand, out)
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
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        kv::Inner::iter_op_spread(
            &borrow.index,
            borrow.epoch,
            &borrow.dict,
            strand,
            context,
            sink,
        )
    }
}

// ── Unpack ──────────────────────────────────────────────────────────

pub(crate) struct Unpack<'v>(kv::UnpackInner<'v, Dict<'v>>);

/// Creates the lazy rest over a dict's leftover pairs.
fn make_unpack<'v>(
    strand: &mut Strand<'v, '_>,
    container: GcObj<'v, Dict<'v>>,
    epoch: u64,
    state: UnpackState<'v>,
    keyed: bool,
) -> Value<'v> {
    Value::from_object(GcObj::new(
        strand.arena(),
        strand.builtin_types().dict_unpack,
        Unpack(kv::UnpackInner {
            state,
            kv: container,
            epoch,
            keyed,
        }),
    ))
}

impl<'v> AsMut<kv::UnpackInner<'v, Dict<'v>>> for Unpack<'v> {
    fn as_mut(&mut self) -> &mut kv::UnpackInner<'v, Dict<'v>> {
        &mut self.0
    }
}

unsafe impl<'v> Collect for Unpack<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.0.accept(visit)
    }

    fn clear(&mut self) {
        self.0.clear()
    }
}

impl<'v> Protocol<'v> for Unpack<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dict unpack iter>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        kv::UnpackInner::op_unpack(this, strand, sig, out, make_unpack).await
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        kv::UnpackInner::op_next(this, strand, out)
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
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        kv::UnpackInner::op_spread(this, strand, context, sink)
    }
}

// ── Protocol: Dict ──────────────────────────────────────────────────

impl<'v> Protocol<'v> for Dict<'v> {
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
            return Err(Error::type_error(strand, "unsupported dict format option"));
        }
        let mut pad = Pad::new(*spec, w);
        if spec.alt {
            kv::Inner::op_format_pretty(this, strand, kind, &mut pad)?;
        } else {
            match kind {
                Kind::Str => Self::op_display(this, strand, &mut pad)?,
                Kind::Dbg => Self::op_debug(this, strand, &mut pad)?,
                Kind::Verbatim => Self::op_verbatim(this, strand, &mut pad)?,
                _ => unreachable!(),
            }
        }
        pad.finish(strand)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().dict)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_debug(this, strand, w, "{", "}", ", ", "")
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let Ok(borrow) = this.borrow(strand) else {
            return true;
        };
        borrow.0.total_pairs != 0
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_hash(this, strand, hasher, sym::DICT)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().dict) {
            other
        } else {
            return Ok(Value::FALSE);
        };
        kv::Inner::op_eq(this, strand, &other)
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().dict) {
            other
        } else {
            return Err(Error::not_supported(strand));
        };
        kv::Inner::op_lt(this, strand, &other)
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_index(this, strand, index, out)
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_assign(this, strand, key, value)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CLEAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                kv::Inner::mcall_clear(this, strand)
            }
            sym::INSERT => {
                let ([key, value], []) = unpack!(strand, args, 2, 0)?;
                kv::Inner::mcall_insert(this, strand, key, value)
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([key], [subindex, default, or_else]) =
                    unpack!(strand, args, 1, 1, default = None, else_key = None)?;
                kv::Inner::mcall_get(this, strand, key, subindex, default, or_else, out).await
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([key], [subindex, default, or_else]) =
                    unpack!(strand, args, 1, 1, default = None, else_key = None)?;
                kv::Inner::mcall_pop(this, strand, key, subindex, default, or_else, out).await
            }
            sym::DELETE => {
                let ([key], _) = unpack!(strand, args, 1, 0)?;
                kv::Inner::mcall_delete(this, strand, key, out)
            }
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                Self::op_iter(this, strand, out).await
            }
            sym::KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let epoch = borrow.0.epoch;
                let dict = this.to_strong();
                strand.builtin_types().dict_keys.create(
                    strand,
                    kv::Keys {
                        index: Cell::new(0),
                        epoch,
                        visited: RefCell::new(bitbox![0; borrow.0.inner.buckets()]),
                        container: dict,
                    },
                    out,
                );
                Ok(())
            }
            sym::VALUES => {
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                let epoch = this.borrow(strand)?.0.epoch;
                let dict = this.to_strong();
                let value = if let Some(key) = key {
                    let hv = kv::hash(strand, &key)?;
                    let bucket = this.borrow(strand)?.0.inner.find(hv, kv::eq(strand, &key));
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().dict_key_values,
                        kv::KeyValues {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                            bucket,
                        },
                    ))
                } else {
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().dict_values,
                        kv::Values {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                        },
                    ))
                };
                out.store(value);
                Ok(())
            }
            sym::COUNT => {
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                kv::Inner::mcall_count(this, strand, key, out)
            }
            sym::COPY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.delegator().is_some() {
                    return strand
                        .with_slots(async |strand, [mut receiver, mut ty]| {
                            Output::set(strand, Slot::reborrow(&mut receiver), &this);
                            receiver.op_type(strand, Slot::reborrow(&mut ty));
                            call!(strand, &ty, out, &receiver).await
                        })
                        .await;
                }
                let borrow = this.borrow(strand)?;
                let mut dict = Dict::new();
                for entry in borrow.0.index.iter().flatten() {
                    let (bucket, subindex) = entry;
                    let bucket = unsafe { bucket.as_ref() };
                    dict.insert(
                        strand,
                        bucket.key.dup(),
                        bucket.value.at(*subindex).dup(),
                        bucket.hash,
                        false,
                    );
                }
                strand.builtin_types().dict.create(strand, dict, out);
                Ok(())
            }
            sym::CONTAINS => {
                let ([key], [value]) = unpack!(strand, args, 1, 1)?;
                kv::Inner::mcall_contains(this, strand, key, value, out)
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "dict.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
                    .await
                    .expect("supported special method")
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
            sym::LEN => {
                let input = this.borrow(strand)?.0.total_pairs;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::CLEAR
            | sym::INSERT
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COUNT
            | sym::COPY
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = Iter {
            index: Cell::new(0),
            dict: this.to_strong(),
            epoch: this.borrow(strand)?.0.epoch,
        };
        strand
            .vm()
            .builtin_types()
            .dict_iter
            .create(strand, iter, out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_spread(this, strand, context, sink).await
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_unpack(this, strand, sig, out, make_unpack)
    }
}

// ── Dict Class ──────────────────────────────────────────────────

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
    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        let mut dict = Dict::new();

        // FIXME: `Dict` is not GC-scannable, but then again if it were it would also
        // be mutably borrowed, which would inhibit GC.  This needs a resolution.
        let mut sink = DictPairs {
            int: 0,
            dict: &mut dict,
        };
        items
            .op_spread(strand, SpreadContext::Pairs, &mut sink)
            .await?;
        strand.builtin_types().dict.create(strand, dict, out);
        Ok(())
    }

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
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Dict>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::CLEAR),
                Method(sym::INSERT),
                Method(sym::GET),
                Method(sym::POP),
                Method(sym::DELETE),
                Method(sym::PAIRS),
                Method(sym::KEYS),
                Method(sym::VALUES),
                Method(sym::COUNT),
                Method(sym::COPY),
                Method(sym::CONTAINS),
                Method(sym::INDEX_METHOD),
                Method(sym::ASSIGN_METHOD),
                Method(sym::ITER_METHOD),
                Method(sym::UNPACK_METHOD),
                Method(sym::SPREAD_METHOD),
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
            | sym::CLEAR
            | sym::COUNT
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COPY
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD
            | sym::SPREAD_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
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
                let ([self_val, items], []) = unpack!(strand, args, 2, 0)?;
                strand
                    .with_slots(async |strand, [mut native]| {
                        call!(strand, &strand.singletons().dict, &mut native, items).await?;
                        self_val.op_fill(strand, &strand.singletons().dict, native.take())?;
                        Ok(())
                    })
                    .await
            }
            _ => type_mcall_fallback(strand, &strand.singletons().dict, method, args, out).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::ErrorKind,
        test_support::{args_from_slots, with_vm},
        value::Value,
    };

    use super::*;

    /// Populates `out` (which must be a GC-rooted slot, e.g. one from [`with_vm`]) with a
    /// freshly built `Dict` containing `pairs`. Values are never returned as bare,
    /// unrooted locals — every call site roots the dict for as long as it's needed by
    /// reading it back in place from the same slot (see `Slot::into_inner`), rather than
    /// duplicating it out into a separate Rust local.
    fn make_dict<'v, 's>(strand: &mut Strand<'v, 's>, pairs: &[(i64, i64)], out: Slot<'v, '_>) {
        let mut dict = Dict::new();
        for &(k, v) in pairs {
            let key = Value::from_i64(strand, k);
            let value = Value::from_i64(strand, v);
            let hv = kv::hash(strand, &key).unwrap();
            dict.insert(strand, key, value, hv, false);
        }
        strand.builtin_types().dict.create(strand, dict, out);
    }

    fn total_pairs<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> usize {
        strand
            .builtin_types()
            .dict
            .cast(value)
            .unwrap()
            .enter_sync(strand, |strand, recv| {
                recv.borrow(strand).unwrap().0.total_pairs
            })
    }

    #[test]
    fn dict_pairs_positional_overflow_errors() {
        with_vm(async |strand, [mut slot]| {
            let mut dict = Dict::new();
            let mut sink = DictPairs {
                int: i64::MAX,
                dict: &mut dict,
            };
            slot.store(Value::from_i64(strand, 0));
            let err = sink.positional(strand, slot).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Overflow);
        });
    }

    #[test]
    fn op_bool_treats_broken_borrow_as_true() {
        with_vm(async |strand, [mut slot]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv1| {
                    let _held = recv1.borrow_mut(strand).unwrap();
                    dict_type
                        .cast(value)
                        .unwrap()
                        .enter_sync(strand, |strand, recv2| {
                            assert!(Dict::op_bool(recv2, strand));
                        });
                });
        });
    }

    #[test]
    fn op_bool_false_when_empty_true_when_nonempty() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot0));
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot1));
            let empty: &Value = &slot0;
            let nonempty: &Value = &slot1;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(empty)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(!Dict::op_bool(recv, strand));
                });
            dict_type
                .cast(nonempty)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(Dict::op_bool(recv, strand));
                });
        });
    }

    #[test]
    fn op_eq_and_op_lt_type_mismatch_are_asymmetric() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot0));
            slot1.store(Value::from_i64(strand, 42));
            let value: &Value = &slot0;
            let other: &Value = &slot1;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let eq = Dict::op_eq(recv, strand, other).unwrap();
                    assert!(!eq.to_bool(strand));
                });
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    match Dict::op_lt(recv, strand, other) {
                        Err(err) => assert_eq!(err.kind(), ErrorKind::Unsupported),
                        Ok(_) => panic!("expected op_lt to error on type mismatch"),
                    }
                });
        });
    }

    #[test]
    fn op_mcall_len_errors_as_field_not_method() {
        with_vm(async |strand, [mut slot0, slot1]| {
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.len()` takes no arguments — a zero-length, separately
                    // rooted `Slots` backs the (empty) `Args`.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            match Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::LEN),
                                args,
                                slot1,
                            )
                            .await
                            {
                                Err(err) => assert_eq!(err.kind(), ErrorKind::Type),
                                Ok(()) => panic!("expected dict.len() as a method call to error"),
                            }
                        })
                        .await;
                })
                .await;
        });
    }

    #[test]
    fn op_mcall_values_with_missing_key_yields_empty_key_values() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.values(key)` takes exactly the one positional key
                    // argument — a separately rooted, single-cell `Slots` backs it.
                    strand
                        .with_slots_dynamic(1, async |strand, mut arg_slots| {
                            arg_slots.at(0).store(Value::from_i64(strand, 999));
                            let sig = [None];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::VALUES),
                                args,
                                Slot::reborrow(&mut slot1),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                })
                .await;
            let out: &Value = &slot1;
            assert!(
                out.downcast_ref(strand.builtin_types().dict_key_values)
                    .is_some()
            );
        });
    }

    #[test]
    fn op_mcall_copy_is_independent_of_original() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[(1, 2), (3, 4)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.copy()` takes no arguments — a zero-length, separately
                    // rooted `Slots` backs the (empty) `Args`.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::COPY),
                                args,
                                Slot::reborrow(&mut slot1),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                })
                .await;

            let copy: &Value = &slot1;
            assert_eq!(total_pairs(strand, copy), 2);

            // Mutate the original after copying; the copy must be unaffected.
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let key = Value::from_i64(strand, 5);
                    let val = Value::from_i64(strand, 6);
                    let hv = kv::hash(strand, &key).unwrap();
                    recv.borrow_mut(strand)
                        .unwrap()
                        .0
                        .insert(strand, key, val, hv, false);
                });

            assert_eq!(total_pairs(strand, copy), 2);
        });
    }
}
