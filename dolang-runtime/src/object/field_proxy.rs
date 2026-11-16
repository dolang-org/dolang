use std::{collections::VecDeque, ops::ControlFlow};

use crate::value::fmt::Format;

use bitvec::bitbox;

use crate::{
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{sym::SymObj, tuple},
    strand::Strand,
    sym::Sym,
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

impl<'v> Protocol<'v> for FieldProxy<'v> {
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
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_mcall(strand, &this, method, args, out).await
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let symbol = index.as_sym(strand).ok_or_else(|| Error::index(strand))?;
        let fields = this.borrow(strand)?;
        if !fields.symbols.iter().any(|key| key.tag() == symbol.tag()) {
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
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Iterable)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<fields>")
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

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<field iterator>")
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
