use std::{collections::VecDeque, ops::ControlFlow};

use crate::value::fmt::Format;

use bitvec::{bitbox, boxed::BitBox};

use crate::{
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{sym::SymObj, tuple},
    sig::{self, UnpackKeyKind},
    strand::Strand,
    sym::Sym,
    value::{Output, Slot, Slots, TypeObject, Value},
};
use dolang_bytecode::{Rest, Variadic};

use super::protocol::{GcObj, MemberKind, Protocol, Recv, Spread, SpreadContext};

/// A live field proxy, or a read-only rest over its unmatched readable fields.
pub(crate) struct FieldIter<'v> {
    receiver: Value<'v>,
    symbols: VecDeque<GcObj<'v, SymObj>>,
    writable: Vec<GcObj<'v, SymObj>>,
}

impl<'v> FieldIter<'v> {
    pub(crate) fn new(receiver: Value<'v>, symbols: VecDeque<GcObj<'v, SymObj>>) -> Self {
        Self {
            receiver,
            symbols,
            writable: Vec::new(),
        }
    }
}

unsafe impl<'v> Collect for FieldIter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
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

fn find_symbol<'v>(symbols: &[GcObj<'v, SymObj>], wanted: Sym<'v, '_>) -> Option<usize> {
    symbols
        .binary_search_by_key(&wanted.tag(), |symbol| symbol.tag)
        .ok()
}

impl<'v> Protocol<'v> for FieldIter<'v> {
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
        if !fields.symbols.iter().any(|key| key.tag == symbol.tag()) {
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
        if !fields.writable.iter().any(|key| key.tag == symbol.tag()) {
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
        strand.builtin_types().field_pairs.create(
            strand,
            FieldPairs {
                fields: FieldIter::new(fields.receiver.dup(), fields.symbols.clone()),
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
        Output::set(strand, out, TypeObject::Iterable)
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
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pos_count = sig.required + sig.optional.len();
        if sig.required != 0 {
            return Err(Error::missing_positional(strand, 0));
        }

        strand
            .with_slots_dynamic(sig.len(), async |strand, mut staged| {
                for (index, default) in sig.optional.iter().enumerate() {
                    staged.at(index).store(default.dup());
                }

                let iter = this.borrow(strand)?;
                let receiver = iter.receiver.dup();
                let symbols: Vec<_> = iter.symbols.iter().cloned().collect();
                let track = sig.key_rest() != Rest::Discard || !sig.keys.is_empty();
                let mut consumed: Option<BitBox> = track.then(|| bitbox![0; symbols.len()]);

                for (key_index, key) in sig.keys.iter().enumerate() {
                    let dest = pos_count + key_index;
                    let found = match &key.kind {
                        UnpackKeyKind::Sym(wanted) => find_symbol(&symbols, *wanted),
                        UnpackKeyKind::Const(_) => None,
                    };
                    if let Some(index) = found {
                        resolve(strand, &receiver, &symbols[index], staged.at(dest))?;
                    } else if let Some(default) = &key.default {
                        staged.at(dest).store(default.dup());
                    } else {
                        return Err(match &key.kind {
                            UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                            UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                        });
                    }
                    if let (Some(index), Some(consumed)) = (found, &mut consumed) {
                        consumed.set(index, true);
                    }
                }

                if sig.key_rest() == Rest::None {
                    let consumed = consumed.as_mut().unwrap();
                    for (index, symbol) in symbols.iter().enumerate() {
                        if consumed[index] {
                            continue;
                        }
                        return Err(Error::unexpected_key(strand, unsafe {
                            Sym::from_obj(symbol)
                        }));
                    }
                }

                let symbols = symbols
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, symbol)| {
                        (!consumed.as_ref().is_some_and(|bits| bits[index])).then_some(symbol)
                    })
                    .collect();
                let child = FieldIter::new(receiver, symbols);
                if sig.variadic == Variadic::Capture {
                    strand.builtin_types().field_iter.create(
                        strand,
                        child,
                        staged.at(sig.len() - 1),
                    );
                } else {
                    if let Some(i) = sig.pos_rest_slot() {
                        staged
                            .at(i)
                            .store(Value::from_object(tuple::tuple(strand, [])));
                    }
                    if let Some(i) = sig.key_rest_slot() {
                        strand
                            .builtin_types()
                            .field_iter
                            .create(strand, child, staged.at(i));
                    }
                }
                for index in 0..sig.len() {
                    out.at(index).store(staged.at(index).take());
                }
                Ok(())
            })
            .await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let iter = this.borrow(strand)?;
        for symbol in iter.symbols.iter().cloned() {
            let receiver = iter.receiver.dup();
            let mut value = strand.with_slots_sync(|strand, [mut value]| {
                resolve(strand, &receiver, &symbol, Slot::reborrow(&mut value))?;
                Ok(value.take())
            })?;
            if context != SpreadContext::Sequence {
                sink.symbol(
                    strand,
                    unsafe { Sym::from_obj(&symbol) },
                    Slot::new(&mut value),
                )?;
            } else {
                value = Value::from_object(tuple::tuple(
                    strand,
                    [Value::from_object(symbol), value.take()],
                ));
                sink.positional(strand, Slot::new(&mut value))?;
            }
        }
        Ok(())
    }
}

/// Inspect the receiver's type once; the proxy roots its receiver and member names.
pub(crate) fn create<'v, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut ty]| {
        receiver.op_type(strand, Slot::reborrow(&mut ty));
        let inspect = ty
            .op_inspect(strand)
            .ok_or_else(|| Error::not_supported(strand))?;
        let mut fields = FieldIter::new(receiver.dup(), VecDeque::new());
        for member in inspect.members {
            if strand.sym_obj(member.sym).private {
                continue;
            }
            if matches!(member.kind, MemberKind::Getter | MemberKind::Property) {
                fields.symbols.push_back(strand.sym_obj(member.sym));
            }
            if matches!(member.kind, MemberKind::Setter | MemberKind::Property) {
                fields.writable.push(strand.sym_obj(member.sym));
            }
        }
        fields.symbols.make_contiguous().sort_by_key(|key| key.tag);
        strand
            .builtin_types()
            .field_iter
            .create(strand, fields, out);
        Ok(())
    })
}

pub(crate) struct FieldPairs<'v> {
    fields: FieldIter<'v>,
    index: usize,
}

unsafe impl<'v> Collect for FieldPairs<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
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
        Output::set(strand, out, TypeObject::Iter)
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
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let Some(symbol) = iter.fields.symbols.get(iter.index).cloned() else {
            return Ok(false);
        };
        let receiver = iter.fields.receiver.dup();
        strand.with_slots_sync(|strand, [mut value]| {
            resolve(strand, &receiver, &symbol, Slot::reborrow(&mut value))?;
            out.store(Value::from_object(tuple::tuple(
                strand,
                [Value::from_object(symbol), value.take()],
            )));
            Ok(())
        })?;
        iter.index += 1;
        Ok(true)
    }
}
