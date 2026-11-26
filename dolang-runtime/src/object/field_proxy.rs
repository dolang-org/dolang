use std::{collections::VecDeque, ops::ControlFlow};

use bitvec::bitbox;

use crate::{
    arg::Args,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{BoundMethod, sym::SymObj, tuple},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Slot, TypeObject, Value},
    vm::AllocExt,
    with_slots,
};
use dolang_bytecode::Rest;

use super::protocol::{
    GcObj, MemberKind, Protocol, Recv, Spread, SpreadContext, Unpack, UnpackItem,
};

/// A live field proxy, or a read-only rest over its unmatched readable fields.
pub(crate) struct FieldProxy<'v> {
    receiver: Value<'v>,
    symbols: VecDeque<GcObj<'v, SymObj>>,
    writable: Vec<GcObj<'v, SymObj>>,
}

impl<'v> FieldProxy<'v> {
    pub(crate) fn new(receiver: Value<'v>, symbols: VecDeque<GcObj<'v, SymObj>>) -> Self {
        Self {
            receiver,
            symbols,
            writable: Vec::new(),
        }
    }
}

unsafe impl<'v> Collect for FieldProxy<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.receiver.accept(visit)?;
        for symbol in &self.symbols {
            symbol.accept(visit)?;
        }
        for symbol in &self.writable {
            symbol.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.receiver = Value::NIL;
        self.symbols.clear();
        self.writable.clear();
    }
}

fn resolve<'v, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: &Value<'v>,
    symbol: &GcObj<'v, SymObj>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    receiver.op_get(strand, unsafe { Sym::from_obj(symbol) }, out)
}

fn find_symbol<'v>(symbols: &VecDeque<GcObj<'v, SymObj>>, wanted: Sym<'v, '_>) -> Option<usize> {
    symbols
        .binary_search_by_key(&wanted.tag(), |symbol| symbol.tag())
        .ok()
}

/// The index of the readable member `key` names, if any
fn find_key<'v>(
    strand: &Strand<'v, '_>,
    symbols: &VecDeque<GcObj<'v, SymObj>>,
    key: &Value<'v>,
) -> Option<usize> {
    find_symbol(symbols, key.as_sym(strand)?)
}

/// Stores an iterator over the pairs of `fields` into `out`.
fn create_pairs<'v>(strand: &mut Strand<'v, '_>, fields: &FieldProxy<'v>, out: Slot<'v, '_>) {
    let embryo = strand.builtin_types().field_pairs.embryo(strand);
    // SAFETY: the symbols and the receiver move straight into the allocated iterator
    let symbols = fields.symbols.iter().map(|s| unsafe { s.dup() }).collect();
    let receiver = unsafe { fields.receiver.dup() };
    embryo.finish(
        strand,
        FieldPairs {
            fields: FieldProxy::new(receiver, symbols),
            index: 0,
        },
        out,
    );
}

impl<'v> Protocol<'v> for FieldProxy<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "FieldProxy";

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let len = this.borrow(strand)?.symbols.len();
                strand.store(out, len);
                Ok(())
            }
            sym::CONTAINS | sym::COUNT | sym::GET | sym::KEYS | sym::VALUES | sym::PAIRS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => super::iter::iterable_get(strand, &this, field, out),
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::LEN => Err(Error::type_error(
                strand,
                "fields len is a field, not a method",
            )),
            sym::CONTAINS => {
                unpack!(strand, args => key, ?value);
                let fields = this.borrow(strand)?;
                let found = match find_key(strand, &fields.symbols, &key) {
                    None => false,
                    Some(_) if value.is_none() => true,
                    Some(index) => with_slots!(strand, |mut member| {
                        resolve(
                            strand,
                            &fields.receiver,
                            &fields.symbols[index],
                            Slot::reborrow(&mut member),
                        )?;
                        member.eq(strand, value.as_deref().unwrap())?
                    }),
                };
                strand.store(out, found);
                Ok(())
            }
            sym::COUNT => {
                unpack!(strand, args => ?key);
                let fields = this.borrow(strand)?;
                let count = match key {
                    Some(key) => usize::from(find_key(strand, &fields.symbols, &key).is_some()),
                    None => fields.symbols.len(),
                };
                strand.store(out, count);
                Ok(())
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => key, ?instance, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let instance = match instance {
                    Some(s) => s.to_i64(strand).map_err(|_| Error::index(strand))?,
                    None => -1,
                };
                // A member has a single value, so only the first or last instance exists
                let found = {
                    let fields = this.borrow(strand)?;
                    match find_key(strand, &fields.symbols, &key) {
                        Some(index) if matches!(instance, 0 | -1) => {
                            resolve(
                                strand,
                                &fields.receiver,
                                &fields.symbols[index],
                                Slot::reborrow(&mut out),
                            )?;
                            true
                        }
                        _ => false,
                    }
                };
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
            sym::KEYS => {
                unpack!(strand, args =>);
                let fields = this.borrow(strand)?;
                strand.new_array(&mut out);
                let array = out.as_array(strand).unwrap();
                for symbol in &fields.symbols {
                    array.push(strand, symbol)?;
                }
                Ok(())
            }
            sym::VALUES => {
                unpack!(strand, args => ?key);
                let fields = this.borrow(strand)?;
                let range = match key {
                    Some(key) => match find_key(strand, &fields.symbols, &key) {
                        Some(index) => index..index + 1,
                        None => 0..0,
                    },
                    None => 0..fields.symbols.len(),
                };
                strand.new_array(&mut out);
                let array = out.as_array(strand).unwrap();
                with_slots!(strand, |mut value| {
                    for symbol in fields.symbols.range(range) {
                        resolve(strand, &fields.receiver, symbol, Slot::reborrow(&mut value))?;
                        array.push(strand, &value)?;
                    }
                    Ok(())
                })
            }
            sym::PAIRS => {
                unpack!(strand, args =>);
                let fields = this.borrow(strand)?;
                create_pairs(strand, &fields, out);
                Ok(())
            }
            _ => super::iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let symbol = index.as_sym(strand).ok_or_else(|| Error::index(strand))?;
        let fields = this.borrow(strand)?;
        if find_symbol(&fields.symbols, symbol).is_none() {
            return Err(Error::index(strand));
        }
        fields.receiver.op_get(strand, symbol, out)
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let symbol = index.as_sym(strand).ok_or_else(|| Error::index(strand))?;
        let fields = this.borrow(strand)?;
        if !fields.writable.iter().any(|key| key.tag() == symbol.tag()) {
            return Err(Error::index(strand));
        }
        fields.receiver.op_set(strand, symbol, value)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let fields = this.borrow(strand)?;
        create_pairs(strand, &fields, out);
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Iterable)
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key_rest = unpack.key_rest();
        let iter = this.borrow(strand)?;
        let receiver = &iter.receiver;
        let symbols = &iter.symbols;
        let mut consumed = bitbox![0; symbols.len()];
        let mut position = 0;
        let mut child = None;
        for entry in unpack.iter() {
            let (found, slot, default) = match entry {
                UnpackItem::Pos { slot, default } => {
                    match default {
                        Some(default) => strand.store(slot, default),
                        None => return Err(Error::missing_positional(strand, position)),
                    }
                    position += 1;
                    continue;
                }
                UnpackItem::SymKey { key, slot, default } => match find_symbol(symbols, key) {
                    Some(index) => (Some(index), slot, default),
                    None if default.is_some() => (None, slot, default),
                    None => return Err(Error::missing_key(strand, key)),
                },
                // Fields are named by symbols
                UnpackItem::ConstKey { key, slot, default } => match default {
                    Some(default) => {
                        strand.store(slot, default);
                        continue;
                    }
                    None => return Err(Error::missing_key(strand, key)),
                },
                UnpackItem::PosRest { slot } => {
                    tuple::create(strand, [], slot);
                    continue;
                }
                UnpackItem::Rest { slot } | UnpackItem::KeyRest { slot } => {
                    child = Some(slot);
                    continue;
                }
            };
            match (found, default) {
                (Some(index), _) => {
                    resolve(strand, receiver, &symbols[index], slot)?;
                    consumed.set(index, true);
                }
                (None, Some(default)) => strand.store(slot, default),
                (None, None) => unreachable!("missing keys are reported above"),
            }
        }

        if key_rest == Rest::None
            && let Some(index) = consumed.first_zero()
        {
            return Err(Error::unexpected_key(strand, unsafe {
                Sym::from_obj(&symbols[index])
            }));
        }

        if let Some(slot) = child {
            let embryo = strand.builtin_types().field_proxy.embryo(strand);
            // SAFETY: the symbols move straight into the allocated proxy
            let symbols = symbols
                .iter()
                .zip(consumed.iter().by_vals())
                .filter(|(_, consumed)| !*consumed)
                .map(|(symbol, _)| unsafe { symbol.dup() })
                .collect();
            // SAFETY: as for the symbols
            let receiver = unsafe { receiver.dup() };
            embryo.finish(strand, FieldProxy::new(receiver, symbols), slot);
        }
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let iter = this.borrow(strand)?;
        with_slots!(strand, |mut value| {
            for symbol in &iter.symbols {
                resolve(strand, &iter.receiver, symbol, Slot::reborrow(&mut value))?;
                sink.symbol(
                    strand,
                    unsafe { Sym::from_obj(symbol) },
                    Slot::reborrow(&mut value),
                )?;
            }
            Ok(())
        })
    }
}

/// Inspect the receiver's type once; the proxy roots its receiver and member names.
pub(crate) fn create<'v, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut ty| {
        receiver.op_type(strand, Slot::reborrow(&mut ty));
        let inspect = ty
            .op_inspect(strand)
            .ok_or_else(|| Error::unsupported(strand, "field iteration"))?;
        let embryo = strand.builtin_types().field_proxy.embryo(strand);
        let mut symbols = VecDeque::new();
        let mut writable = Vec::new();
        for member in inspect.members {
            if strand.sym_private(member.sym) {
                continue;
            }
            // SAFETY (both): the iterator is allocated, and nothing allocates before
            // the names move into it below
            if matches!(member.kind, MemberKind::Getter | MemberKind::Property) {
                symbols.push_back(unsafe { strand.sym_obj(member.sym) });
            }
            if matches!(member.kind, MemberKind::Setter | MemberKind::Property) {
                writable.push(unsafe { strand.sym_obj(member.sym) });
            }
        }
        symbols.make_contiguous().sort_by_key(|key| key.tag());
        // SAFETY: the iterator is allocated; the receiver and member names move straight
        // into it
        let fields = FieldProxy {
            receiver: unsafe { receiver.dup() },
            symbols,
            writable,
        };
        embryo.finish(strand, fields, out);
        Ok(())
    })
}

pub(crate) struct FieldPairs<'v> {
    fields: FieldProxy<'v>,
    index: usize,
}

unsafe impl<'v> Collect for FieldPairs<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.fields.accept(visit)
    }

    fn clear(&mut self) {
        self.fields.clear();
    }
}

impl<'v> Protocol<'v> for FieldPairs<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "FieldPairs";

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_mcall(strand, &this, method, args, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Iter)
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
        let mut iter = this.borrow_mut(strand)?;
        let Some(symbol) = iter.fields.symbols.get(iter.index) else {
            return Ok(false);
        };
        let receiver = &iter.fields.receiver;
        with_slots!(strand, |mut value| {
            resolve(strand, receiver, symbol, Slot::reborrow(&mut value))?;
            tuple::create_pair(strand, symbol, &value, out);
            Ok(())
        })?;
        iter.index += 1;
        Ok(true)
    }
}
