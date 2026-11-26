use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::Hash,
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::{Unpack, members},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Value},
    vm::{AllocExt, Vm},
    with_slots,
};

use super::{
    BoundMethod,
    pack::{self, ArgItem},
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        type_mcall_fallback,
    },
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
    /// The name of the first repeated key among a struct's symbol `keys`, given as tags
    /// and names.
    fn duplicate_key<'k>(keys: impl IntoIterator<Item = (sym::Tag, &'k str)>) -> Option<&'k str> {
        let mut seen = HashSet::new();
        keys.into_iter()
            .find(|(tag, _)| !seen.insert(*tag))
            .map(|(_, name)| name)
    }

    fn duplicate_key_error<'s>(strand: &mut Strand<'v, 's>, name: &str) -> Error<'v, 's> {
        Error::value(strand, format!("duplicate struct key: {name}"))
    }

    /// Store a struct of `args`, whose keys must be unique, into `out`.
    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let vm = strand.vm();
        let keys = args.reborrow().filter_map(|arg| match arg {
            Arg::Key(key, _) => Some((key.tag(), key.as_str(vm))),
            Arg::Pos(_) => None,
        });
        if let Some(name) = Self::duplicate_key(keys) {
            return Err(Self::duplicate_key_error(strand, name));
        }
        let embryo = strand.builtin_types().structure.embryo(strand);
        // SAFETY: the struct is allocated; the items move straight into it
        let items = unsafe { pack::pack_items(strand, args) };
        embryo.finish(strand, Self { items }, out);
        Ok(())
    }

    pub(crate) fn items(&self) -> &[ArgItem<'v>] {
        &self.items
    }

    /// The index of the item keyed `key`
    fn position(&self, key: Sym<'v, '_>) -> Option<usize> {
        self.items
            .iter()
            .position(|(k, _)| k.as_ref().is_some_and(|k| k.tag() == key.tag()))
    }

    /// Replaces the item keyed `key`, or appends one.
    fn set(&mut self, key: GcObj<'v, SymObj>, value: Value<'v>) {
        match self.position(unsafe { Sym::from_obj(&key) }) {
            Some(index) => self.items[index].1 = value,
            None => self.items.push((Some(key), value)),
        }
    }
}

/// Store a `Pack` holding the items of `this` as they are now into `out`, for iteration
/// and unpacking.
fn snapshot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: &Recv<'v, '_, Struct<'v>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    let embryo = strand.builtin_types().pack.embryo(strand);
    // SAFETY: the pack is allocated; the copies move straight into it
    let items = borrow
        .items
        .iter()
        .map(|(key, value)| unsafe { (key.as_ref().map(|key| key.dup()), value.dup()) })
        .collect();
    embryo.finish_imm(strand, pack::Pack::new(items), out);
    Ok(())
}

impl<'v> Protocol<'v> for Struct<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Struct";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().structure)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "Struct")?;
        pack::debug_items(strand, &borrow.items, false, w)
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
        pack::hash_items(strand, &borrow.items, hasher)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().structure) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let flag = pack::eq_items(strand, &left.items, &right.items)?;
            strand.store(&mut out, flag);
            Ok(())
        })
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().structure) else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let flag = pack::lt_items(strand, &left.items, &right.items)?;
            strand.store(&mut out, flag);
            Ok(())
        })
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
        match pack::find(strand, &borrow.items, index) {
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
        key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(key) = key.as_sym(strand) {
            let mut borrow = this.borrow_mut(strand)?;
            // SAFETY: the symbol and the value move straight into the borrowed struct
            let key = unsafe { strand.sym_obj(key) };
            borrow.set(key, unsafe { value.take(strand) });
            return Ok(());
        }
        if !key.is_int(strand) {
            return Err(Error::type_error(
                strand,
                "structs only support symbol and integer keys",
            ));
        }
        let mut borrow = this.borrow_mut(strand)?;
        match pack::find_index(strand, &borrow.items, &key) {
            Some(index) => {
                // SAFETY: the value moves straight into the borrowed struct
                borrow.items[index].1 = unsafe { value.take(strand) };
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
        let borrow = this.borrow(strand)?;
        match borrow.position(field) {
            Some(index) => {
                strand.store(out, &borrow.items[index].1);
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
        let mut borrow = this.borrow_mut(strand)?;
        // SAFETY: the symbol and the value move straight into the borrowed struct
        let key = unsafe { strand.sym_obj(field) };
        borrow.set(key, unsafe { value.take(strand) });
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // A plain name calls its item
        with_slots!(strand, |mut slot| {
            {
                let borrow = this.borrow(strand)?;
                let Some(index) = borrow.position(method) else {
                    return Err(Error::field(strand, method));
                };
                strand.store(&mut slot, &borrow.items[index].1);
            }
            slot.call(strand, args, out).await
        })
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut items| {
            snapshot(strand, &this, &mut items)?;
            let items = items
                .downcast_ref(strand.builtin_types().pack)
                .expect("pack");
            let embryo = strand.builtin_types().pack_iter.embryo(strand);
            // SAFETY: the iterator is allocated; the copy of the rooted snapshot moves
            // straight into it
            let items = unsafe { items.dup() };
            embryo.finish(strand, pack::Iter::new(items, HashSet::new(), 0, 0), out);
            Ok(())
        })
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        pack::spread_items(strand, &borrow.items, sink)
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut items| {
            snapshot(strand, &this, &mut items)?;
            let items = items
                .downcast_ref(strand.builtin_types().pack)
                .expect("pack");
            pack::unpack(strand, items, unpack)
        })
    }
}

// ── Struct Class ────────────────────────────────────────────────────

pub(crate) struct Class;

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Class {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Struct";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
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
                Method(sym::GET),
                Method(sym::CONTAINS),
            ],
            members: members![
                ..Value,
                Method(sym::INIT_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::LT_METHOD),
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
                unpack!(strand, args => self_val, items);
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().structure, items, &mut native)?;
                    self_val.op_fill(
                        strand,
                        &strand.singletons().structure,
                        Slot::reborrow(&mut native),
                    )?;
                    Ok(())
                })
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => target, key, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let found = with_pin!(strand, expect_struct(strand, &target)?, |target| {
                    let borrow = target.borrow().ok_or_else(|| Error::concurrency(strand))?;
                    let found = pack::find(strand, &borrow.items, &key);
                    if let Some(value) = found {
                        strand.store(&mut out, value);
                    }
                    Ok(found.is_some())
                })?;
                if found {
                    Ok(())
                } else if let Some(mut default) = default {
                    strand.store(&mut out, &mut default);
                    Ok(())
                } else if let Some(or_else) = or_else {
                    call!(strand, or_else, out)
                } else {
                    strand.store(&mut out, Value::NIL);
                    Ok(())
                }
            }
            sym::CONTAINS => {
                unpack!(strand, args => target, key);
                let found = {
                    with_pin!(strand, expect_struct(strand, &target)?, |target| {
                        let borrow = target.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        Ok(pack::find_index(strand, &borrow.items, &key).is_some())
                    })?
                };
                strand.store(out, found);
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
        unpack!(strand, args => source);
        with_slots!(strand, |mut acc| {
            pack::source_items(strand, &source, Slot::reborrow(&mut acc)).await?;
            let duplicate = with_pin!(
                strand,
                acc.downcast_ref(strand.builtin_types().arg_acc)
                    .expect("source accumulator"),
                |acc| {
                    let acc = acc.borrow().expect("private accumulator");
                    let keys = acc.items().iter().filter_map(|(key, _)| key.as_ref());
                    Struct::duplicate_key(keys.map(|key| (key.tag(), key.name())))
                        .map(str::to_owned)
                }
            );
            if let Some(name) = duplicate {
                return Err(Struct::duplicate_key_error(strand, &name));
            }
            let embryo = strand.builtin_types().structure.embryo(strand);
            let items = pack::take_source_items(strand, &acc);
            embryo.finish(strand, Struct { items }, out);
            Ok(())
        })
    }
}

/// `target` as a struct, for a class method
fn expect_struct<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    target: &'a Value<'v>,
) -> Result<'v, 's, &'a GcObj<'v, Struct<'v>>> {
    target
        .downcast_native(strand.vm(), strand.builtin_types().structure)
        .ok_or_else(|| Error::type_error(strand, "expected `Struct`"))
}
