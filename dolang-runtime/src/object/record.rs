use std::{
    cell::{Cell, RefCell},
    fmt,
    hash::{DefaultHasher, Hasher},
    ops::ControlFlow,
};

use bitvec::bitbox;

use crate::{
    arg::{Arg, Args},
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{self, Output, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, iter,
    kv::{self, Inner, UnpackState},
    protocol::{
        GcObj, GcObjBorrow, Inspect, Protocol, Recv, Spread, SpreadContext, dispatch_native_method,
    },
};

// ── Record newtype ──────────────────────────────────────────────────

pub(crate) struct Record<'v>(pub(crate) Inner<'v>);

impl<'v> AsRef<Inner<'v>> for Record<'v> {
    fn as_ref(&self) -> &Inner<'v> {
        &self.0
    }
}

impl<'v> AsMut<Inner<'v>> for Record<'v> {
    fn as_mut(&mut self) -> &mut Inner<'v> {
        &mut self.0
    }
}

unsafe impl<'v> Collect for Record<'v> {
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

impl<'v> Record<'v> {
    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self(Inner::new());
        let mut counter = 1;
        let mut index = 0;

        loop {
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_interrupt_gc()?
            }
            counter += 1;
            match args.next() {
                Some(Arg::Pos(mut value)) => {
                    let key = Value::from_i64(strand, index);
                    let hv = kv::hash(strand, &key).unwrap();
                    this.0.insert(strand, key, value.take(), hv, false);
                    index += 1;
                }
                Some(Arg::Key(key, mut value)) => {
                    let key = Value::from_object(strand.sym_obj(key));
                    let hv = kv::hash(strand, &key).unwrap();
                    this.0.insert(strand, key, value.take(), hv, false);
                }
                None => break,
            }
        }

        Ok(this)
    }
}

// ── Iter ────────────────────────────────────────────────────────────

pub(crate) struct Iter<'v> {
    index: Cell<usize>,
    epoch: u64,
    record: GcObj<'v, Record<'v>>,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.record.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<record iterator>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        value::Output::set(strand, out, &this);
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
        kv::Inner::iter_op_next(&borrow.index, borrow.epoch, &borrow.record, strand, out)
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
            &borrow.record,
            strand,
            context,
            sink,
        )
    }
}

// ── Unpack ──────────────────────────────────────────────────────────

pub(crate) struct Unpack<'v>(kv::UnpackInner<'v, Record<'v>>);

impl<'v> AsMut<kv::UnpackInner<'v, Record<'v>>> for Unpack<'v> {
    fn as_mut(&mut self) -> &mut kv::UnpackInner<'v, Record<'v>> {
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<record unpack iter>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        value::Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        kv::UnpackInner::op_unpack(this, strand, sig, out).await
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

// ── Protocol: Record ────────────────────────────────────────────────

impl<'v> Protocol<'v> for Record<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().record)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().record.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_debug(this, strand, w, "<record ", ">", " ")
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_hash(this, strand, hasher, sym::RECORD)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().record) {
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
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().record) {
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
        if index.as_i64(strand).is_none() && index.as_sym(strand).is_none() {
            return Err(Error::type_error(
                strand,
                "records only support symbol and integer keys",
            ));
        }
        kv::Inner::op_index(this, strand, index, out)
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if key.as_i64(strand).is_none() && key.as_sym(strand).is_none() {
            return Err(Error::type_error(
                strand,
                "records only support symbol and integer keys",
            ));
        }
        kv::Inner::op_assign(this, strand, key, value)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = DefaultHasher::new();
        std::hash::Hash::hash(&field, &mut hasher);
        let hash = hasher.finish();
        match this
            .borrow(strand)?
            .0
            .inner
            .find(hash, |kv::Entry { key, .. }| {
                key.as_sym(strand) == Some(field)
            }) {
            Some(pair) => {
                value::Output::set(strand, out, unsafe { pair.as_ref().value.at(0) });
                Ok(())
            }
            None => Err(Error::field(strand, field)),
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_object(strand.sym_obj(field));
        let hv = kv::hash(strand, &key).unwrap();
        let mut borrow = this.borrow_mut(strand)?;
        borrow.0.insert(strand, key, value.take(), hv, true);
        Ok(())
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().record_iter,
            Iter {
                index: Cell::new(0),
                record: this.to_strong(),
                epoch: this.borrow(strand)?.0.epoch,
            },
        )));
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
        kv::Inner::op_unpack(this, strand, sig, out, |strand, container, epoch, skip| {
            Value::from_object(GcObj::new(
                strand.arena(),
                strand.builtin_types().record_unpack,
                Unpack(kv::UnpackInner {
                    state: UnpackState::Order {
                        int: sig.required as i64,
                        index: 0,
                        skip,
                    },
                    kv: container,
                    epoch,
                }),
            ))
        })
    }
}

// ── Record Class ────────────────────────────────────────────────────

pub(crate) struct Class;

impl Class {
    fn downcast<'v, 's, 'a>(
        strand: &Strand<'v, 's>,
        value: &'a Value<'v>,
    ) -> Result<'v, 's, GcObjBorrow<'v, 'a, Record<'v>>> {
        if let Some(borrow) = value.downcast_ref(strand.builtin_types().record) {
            Ok(borrow)
        } else {
            Err(Error::type_error(
                strand,
                "record: expected record for first argument",
            ))
        }
    }
}

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Class {
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
        write!(w, "<type std.record>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![
                Sym::well_known(sym::STR_METHOD),
                Sym::well_known(sym::DBG_METHOD),
                Sym::well_known(sym::EQ_METHOD),
                Sym::well_known(sym::LT_METHOD),
                Sym::well_known(sym::HASH_METHOD),
                Sym::well_known(sym::INDEX_METHOD),
                Sym::well_known(sym::ASSIGN_METHOD),
                Sym::well_known(sym::ITER_METHOD),
                Sym::well_known(sym::UNPACK_METHOD),
                Sym::well_known(sym::GET_METHOD),
                Sym::well_known(sym::SET_METHOD),
            ],
        })
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val], []) = unpack!(strand, args, 1, 0)?;
                let native = Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().record,
                    Record(kv::Inner::new()),
                ));
                self_val.op_fill(strand, &strand.vm().singletons().record, native)?;
                Ok(())
            }
            sym::LEN => {
                let ([record], []) = unpack!(strand, args, 1, 0)?;
                let record = Self::downcast(strand, &record)?;
                Output::set(
                    strand,
                    out,
                    record
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?
                        .0
                        .total_pairs as i64,
                );
                Ok(())
            }
            sym::CLEAR => {
                let ([record], []) = unpack!(strand, args, 1, 0)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_clear(Recv::new(record), strand)
            }
            sym::INSERT => {
                let ([record, key, value], []) = unpack!(strand, args, 3, 0)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_insert(Recv::new(record), strand, key, value)
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([record, key], [subindex, default, or_else]) =
                    unpack!(strand, args, 2, 1, default = None, else_key = None)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_get(
                    Recv::new(record),
                    strand,
                    key,
                    subindex,
                    default,
                    or_else,
                    out,
                )
                .await
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([record, key], [subindex, default, or_else]) =
                    unpack!(strand, args, 2, 1, default = None, else_key = None)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_pop(
                    Recv::new(record),
                    strand,
                    key,
                    subindex,
                    default,
                    or_else,
                    out,
                )
                .await
            }
            sym::DELETE => {
                let ([record, key], _) = unpack!(strand, args, 2, 0)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_delete(Recv::new(record), strand, key, out)
            }
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                Self::op_iter(this, strand, out).await
            }
            sym::KEYS => {
                let ([record], []) = unpack!(strand, args, 1, 0)?;
                let record = Self::downcast(strand, &record)?;
                let epoch = record
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .0
                    .epoch;
                let buckets = record
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .0
                    .inner
                    .buckets();
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().record_keys,
                    kv::Keys {
                        index: Cell::new(0),
                        epoch,
                        visited: RefCell::new(bitbox![0; buckets]),
                        container: Recv::new(record).to_strong(),
                    },
                )));
                Ok(())
            }
            sym::VALUES => {
                let ([record], [key]) = unpack!(strand, args, 1, 1)?;
                let record = Self::downcast(strand, &record)?;
                let epoch = record
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .0
                    .epoch;
                let container = Recv::new(record).to_strong();
                let value = if let Some(key) = key {
                    if key.as_i64(strand).is_none() && key.as_sym(strand).is_none() {
                        return Err(Error::type_error(
                            strand,
                            "records only support symbol and integer keys",
                        ));
                    }
                    let hv = kv::hash(strand, &key)?;
                    let bucket = record
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?
                        .0
                        .inner
                        .find(hv, kv::eq(strand, &key));
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().record_key_values,
                        kv::KeyValues {
                            index: Cell::new(0),
                            epoch,
                            container,
                            bucket,
                        },
                    ))
                } else {
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().record_values,
                        kv::Values {
                            index: Cell::new(0),
                            epoch,
                            container,
                        },
                    ))
                };
                out.store(value);
                Ok(())
            }
            sym::COUNT => {
                let ([record], [key]) = unpack!(strand, args, 1, 1)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_count(Recv::new(record), strand, key, out)
            }
            sym::CONTAINS => {
                let ([record, key], [value]) = unpack!(strand, args, 2, 1)?;
                let record = Self::downcast(strand, &record)?;
                kv::Inner::mcall_contains(Recv::new(record), strand, key, value, out)
            }
            _ => {
                let vm = strand.vm();
                dispatch_native_method(strand, &vm.singletons().record, method, args, out).await
            }
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD
            | sym::LEN
            | sym::CLEAR
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COUNT
            | sym::CONTAINS
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD
            | sym::GET_METHOD
            | sym::SET_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let value = Record::from_args(strand, args)?;
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().record,
            value,
        )));
        Ok(())
    }
}
