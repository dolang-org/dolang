use std::{
    cell::{Cell, RefCell},
    fmt,
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
    value::{self, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, iter,
    kv::{self, Inner, UnpackState},
    protocol::{GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, dispatch_native_method},
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<dict iterator>").into_do(strand)
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<dict unpack iter>").into_do(strand)
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

// ── Protocol: Dict ──────────────────────────────────────────────────

impl<'v> Protocol<'v> for Dict<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().dict)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().dict.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        kv::Inner::op_debug(this, strand, w, "{", "}", ", ")
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
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().dict_keys,
                    kv::Keys {
                        index: Cell::new(0),
                        epoch,
                        visited: RefCell::new(bitbox![0; borrow.0.inner.buckets()]),
                        container: dict,
                    },
                )));
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
            sym::CONTAINS => {
                let ([key], [value]) = unpack!(strand, args, 1, 1)?;
                kv::Inner::mcall_contains(this, strand, key, value, out)
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "dict.len is a field, not a method",
            )),
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
                let input = this.borrow(strand)?.0.total_pairs as i64;
                value::Output::set(strand, out, input);
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = Iter {
            index: Cell::new(0),
            dict: this.to_strong(),
            epoch: this.borrow(strand)?.0.epoch,
        };
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().dict_iter,
            iter,
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
                strand.builtin_types().dict_unpack,
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        let mut dict = Dict::new();

        // FIXME: `dict` is not GC-scannable, but then again if it were it would also
        // be mutably borrowed, which would inhibit GC.  This needs a resolution.
        let mut sink = DictPairs {
            int: 0,
            dict: &mut dict,
        };
        items
            .op_spread(strand, SpreadContext::Pairs, &mut sink)
            .await?;
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().dict,
            dict,
        )));
        Ok(())
    }

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
        write!(w, "<type std.dict>").into_do(strand)
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
                Sym::well_known(sym::LEN),
                Sym::well_known(sym::CLEAR),
                Sym::well_known(sym::INSERT),
                Sym::well_known(sym::GET),
                Sym::well_known(sym::POP),
                Sym::well_known(sym::DELETE),
                Sym::well_known(sym::PAIRS),
                Sym::well_known(sym::KEYS),
                Sym::well_known(sym::VALUES),
                Sym::well_known(sym::COUNT),
                Sym::well_known(sym::CONTAINS),
                Sym::well_known(sym::INDEX_METHOD),
                Sym::well_known(sym::ASSIGN_METHOD),
                Sym::well_known(sym::ITER_METHOD),
                Sym::well_known(sym::UNPACK_METHOD),
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
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD => {
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
                let ([self_val], []) = unpack!(strand, args, 1, 0)?;
                let native = Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().dict,
                    Dict::new(),
                ));
                self_val.op_fill(strand, &strand.vm().singletons().dict, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.vm().singletons().dict, method, args, out)
                    .await
            }
        }
    }
}
