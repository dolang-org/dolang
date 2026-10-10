use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::Hash,
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, Value},
    vm::Vm,
};

use super::{
    BoundMethod,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        instance_mcall_fallback, is_special_mcall, is_universal_member, type_mcall_fallback,
    },
    record::{self, ArgItem, Record},
    sym::SymObj,
};

// ── Struct ──────────────────────────────────────────────────────────

/// A mutable sequence of positional and uniquely symbol-keyed items, in the
/// order they were added. Its plain names are its keyed items.
pub(crate) struct Struct<'v> {
    items: Vec<ArgItem<'v>>,
}

unsafe impl<'v> Collect for Struct<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.items {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.items = Vec::new();
    }
}

impl<'v> Struct<'v> {
    /// Builds a struct from `items`, whose symbol keys must be unique.
    pub(crate) fn new<'s>(
        strand: &mut Strand<'v, 's>,
        items: Vec<ArgItem<'v>>,
    ) -> Result<'v, 's, Self> {
        let mut seen = HashSet::new();
        for (key, _) in &items {
            if let Some(key) = key
                && !seen.insert(key.tag)
            {
                return Err(Error::value(
                    strand,
                    format!("duplicate struct key: {}", key.name),
                ));
            }
        }
        Ok(Self { items })
    }

    pub(crate) fn items(&self) -> &[ArgItem<'v>] {
        &self.items
    }

    /// The index of the item keyed `key`
    fn position(&self, key: Sym<'v, '_>) -> Option<usize> {
        self.items
            .iter()
            .position(|(k, _)| k.as_ref().is_some_and(|k| k.tag == key.tag()))
    }

    /// Replaces the item keyed `key`, or appends one.
    fn set(&mut self, key: GcObj<'v, SymObj>, value: Value<'v>) {
        match self.position(unsafe { Sym::from_obj(&key) }) {
            Some(index) => self.items[index].1 = value,
            None => self.items.push((Some(key), value)),
        }
    }
}

/// A record holding the items of `this` as they are now, for iteration and
/// unpacking.
fn snapshot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: &Recv<'v, '_, Struct<'v>>,
) -> Result<'v, 's, GcObj<'v, Record<'v>>> {
    let items = this
        .borrow(strand)?
        .items
        .iter()
        .map(|(key, value)| (key.clone(), value.dup()))
        .collect();
    Ok(GcObj::new(
        strand.vm().arena(),
        strand.builtin_types().record,
        Record::new(items),
    ))
}

impl<'v> Protocol<'v> for Struct<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().structure)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "Struct")?;
        record::debug_items(strand, &borrow.items, w)
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let Ok(borrow) = this.borrow(strand) else {
            return Ok(true);
        };
        Ok(!borrow.items.is_empty())
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        sym::STRUCT.hash(hasher);
        record::hash_items(strand, &borrow.items, hasher)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().structure) else {
            return Ok(Value::FALSE);
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(record::eq_items(
            strand,
            &left.items,
            &right.items,
        )?))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().structure) else {
            return Err(Error::not_supported(strand));
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        Ok(Value::from_bool(record::lt_items(
            strand,
            &left.items,
            &right.items,
        )?))
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !index.is_int(strand) && index.as_sym(strand).is_none() {
            return Err(Error::type_error(
                strand,
                "structs only support symbol and integer keys",
            ));
        }
        let borrow = this.borrow(strand)?;
        match record::find(strand, &borrow.items, index) {
            Some(value) => {
                Output::set(strand, out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(key) = key.as_sym(strand) {
            let key = strand.sym_obj(key);
            this.borrow_mut(strand)?.set(key, value.take());
            return Ok(());
        }
        if !key.is_int(strand) {
            return Err(Error::type_error(
                strand,
                "structs only support symbol and integer keys",
            ));
        }
        let mut borrow = this.borrow_mut(strand)?;
        match record::find_index(strand, &borrow.items, &key) {
            Some(index) => {
                borrow.items[index].1 = value.take();
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if is_universal_member(field) {
            return instance_get_fallback(strand, &this, field, out);
        }
        let borrow = this.borrow(strand)?;
        match borrow.position(field) {
            Some(index) => {
                Output::set(strand, out, &borrow.items[index].1);
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
        let key = strand.sym_obj(field);
        this.borrow_mut(strand)?.set(key, value.take());
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if is_special_mcall(method.tag()) {
            return instance_mcall_fallback(strand, &this, method, args, out).await;
        }
        // A plain name calls its item
        let callee = {
            let borrow = this.borrow(strand)?;
            borrow
                .position(method)
                .map(|index| borrow.items[index].1.dup())
        };
        let Some(callee) = callee else {
            return Err(Error::field(strand, method));
        };
        strand
            .with_slots(async |strand, [mut slot]| {
                slot.store(callee);
                slot.call(strand, args, out).await
            })
            .await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let snapshot = snapshot(strand, &this)?;
        strand.builtin_types().record_iter.create(
            strand,
            record::Iter::new(snapshot, HashSet::new(), 0, 0),
            out,
        );
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        record::spread_items(strand, &borrow.items, context, sink)
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let snapshot = snapshot(strand, &this)?;
        record::unpack(strand, snapshot, sig, out)
    }
}

// ── Struct Class ────────────────────────────────────────────────────

pub(crate) struct Class;

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
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Struct>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
                Method(sym::GET),
                Method(sym::CONTAINS),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::HASH_METHOD),
                Method(sym::INDEX_METHOD),
                Method(sym::ASSIGN_METHOD),
                Method(sym::ITER_METHOD),
                Method(sym::UNPACK_METHOD),
                Method(sym::SPREAD_METHOD),
            ],
        })
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val, items], []) = unpack!(strand, args, 2, 0)?;
                strand
                    .with_slots(async |strand, [mut native]| {
                        call!(strand, &strand.singletons().structure, &mut native, items).await?;
                        self_val.op_fill(strand, &strand.singletons().structure, native.take())?;
                        Ok(())
                    })
                    .await
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([target, key], [default, or_else]) =
                    unpack!(strand, args, 2, 0, default = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let found = {
                    let target = expect_struct(strand, &target)?;
                    let borrow = target.borrow().ok_or_else(|| Error::concurrency(strand))?;
                    record::find(strand, &borrow.items, &key).map(Value::dup)
                };
                if let Some(value) = found {
                    out.store(value);
                    Ok(())
                } else if let Some(mut default) = default {
                    out.store(default.take());
                    Ok(())
                } else if let Some(or_else) = or_else {
                    call!(strand, or_else, out).await
                } else {
                    out.store(Value::NIL);
                    Ok(())
                }
            }
            sym::CONTAINS => {
                let ([target, key], []) = unpack!(strand, args, 2, 0)?;
                let found = {
                    let target = expect_struct(strand, &target)?;
                    let borrow = target.borrow().ok_or_else(|| Error::concurrency(strand))?;
                    record::find_index(strand, &borrow.items, &key).is_some()
                };
                Output::set(strand, out, found);
                Ok(())
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().structure, method, args, out).await
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
            | sym::GET
            | sym::CONTAINS
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD
            | sym::SPREAD_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([source], []) = unpack!(strand, args, 1, 0)?;
        let items = record::source_items(strand, &source, "struct").await?;
        let structure = Struct::new(strand, items)?;
        strand
            .builtin_types()
            .structure
            .create(strand, structure, out);
        Ok(())
    }
}

/// `target` as a struct, for a class method
fn expect_struct<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    target: &'a Value<'v>,
) -> Result<'v, 's, crate::gc::Borrow<'v, 'a, super::protocol::Header, Struct<'v>>> {
    target
        .downcast_native(strand.vm(), strand.builtin_types().structure)
        .ok_or_else(|| Error::type_error(strand, "expected `Struct`"))
}
