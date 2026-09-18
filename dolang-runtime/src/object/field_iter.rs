use std::{collections::VecDeque, ops::ControlFlow};

use crate::value::fmt::Format;

use bitvec::{bitbox, boxed::BitBox};

use crate::{
    error::{Error, ErrorKind, Result},
    gc::{Collect, arena::Visit},
    object::{iter, sym::SymObj, tuple},
    sig::{self, UnpackKeyKind},
    strand::Strand,
    sym::Sym,
    value::{Output, Slot, Slots, Value},
};
use dolang_bytecode::Variadic;

use super::protocol::{GcObj, Protocol, Recv, Spread, SpreadContext};

/// Lazy iterator over the unmatched readable fields of an object.
pub(crate) struct FieldIter<'v> {
    receiver: Value<'v>,
    symbols: VecDeque<GcObj<'v, SymObj>>,
}

impl<'v> FieldIter<'v> {
    pub(crate) fn new(receiver: Value<'v>, symbols: VecDeque<GcObj<'v, SymObj>>) -> Self {
        Self { receiver, symbols }
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
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.receiver = Value::NIL;
        self.symbols.clear();
    }
}

fn resolve<'v, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: &Value<'v>,
    symbol: &GcObj<'v, SymObj>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    let sym = unsafe { Sym::from_obj(symbol) };
    match receiver.op_get(strand, sym, out) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::Field => Ok(false),
        Err(error) => Err(error),
    }
}

fn find_symbol<'v>(symbols: &[GcObj<'v, SymObj>], wanted: Sym<'v, '_>) -> Option<usize> {
    symbols
        .binary_search_by_key(&wanted.tag(), |symbol| symbol.tag)
        .ok()
}

impl<'v> Protocol<'v> for FieldIter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<field iter>")
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
        loop {
            let mut iter = this.borrow_mut(strand)?;
            let Some(symbol) = iter.symbols.front().cloned() else {
                return Ok(false);
            };
            let receiver = iter.receiver.dup();
            let present = strand.with_slots_sync(|strand, [mut value]| {
                let present = resolve(strand, &receiver, &symbol, Slot::reborrow(&mut value))?;
                Ok((present, value.take()))
            })?;
            iter.symbols.pop_front();
            if present.0 {
                out.store(Value::from_object(tuple::tuple(
                    strand,
                    [Value::from_object(symbol), present.1],
                )));
                return Ok(true);
            }
        }
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        sig.reject_split(strand)?;
        let pos_count = sig.required + sig.optional.len();
        if sig.required != 0 {
            return Err(Error::missing_positional(strand, 0));
        }

        strand
            .with_slots_dynamic(sig.len(), async |strand, mut staged| {
                for (index, default) in sig.optional.iter().enumerate() {
                    staged.at(index).store(default.dup());
                }

                let mut iter = this.borrow_mut(strand)?;
                let receiver = iter.receiver.dup();
                let symbols = iter.symbols.make_contiguous();
                let track = sig.variadic != Variadic::Discard || !sig.keys.is_empty();
                let mut consumed: Option<BitBox> = track.then(|| bitbox![0; symbols.len()]);

                for (key_index, key) in sig.keys.iter().enumerate() {
                    let dest = pos_count + key_index;
                    let found = match &key.kind {
                        UnpackKeyKind::Sym(wanted) => find_symbol(symbols, *wanted),
                        UnpackKeyKind::Const(_) => None,
                    };
                    let present = if let Some(index) = found {
                        resolve(strand, &receiver, &symbols[index], staged.at(dest))?
                    } else {
                        false
                    };
                    if !present {
                        if let Some(default) = &key.default {
                            staged.at(dest).store(default.dup());
                        } else {
                            return Err(match &key.kind {
                                UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                                UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                            });
                        }
                    }
                    if let (Some(index), Some(consumed)) = (found, &mut consumed) {
                        consumed.set(index, true);
                    }
                }

                if sig.variadic == Variadic::NONE {
                    let consumed = consumed.as_mut().unwrap();
                    for (index, symbol) in symbols.iter().enumerate() {
                        if consumed[index] {
                            continue;
                        }
                        let present = strand.with_slots_sync(|strand, [tmp]| {
                            resolve(strand, &receiver, symbol, tmp)
                        })?;
                        if present {
                            return Err(Error::unexpected_key(strand, unsafe {
                                Sym::from_obj(symbol)
                            }));
                        }
                        consumed.set(index, true);
                    }
                }

                if let Some(consumed) = &consumed {
                    for index in consumed.iter_ones().rev() {
                        iter.symbols.remove(index);
                    }
                }
                if sig.variadic == Variadic::Capture {
                    Output::set(strand, staged.at(sig.len() - 1), &this);
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
        loop {
            let mut iter = this.borrow_mut(strand)?;
            let Some(symbol) = iter.symbols.front().cloned() else {
                return Ok(());
            };
            let receiver = iter.receiver.dup();
            let (present, mut value) = strand.with_slots_sync(|strand, [mut value]| {
                let present = resolve(strand, &receiver, &symbol, Slot::reborrow(&mut value))?;
                Ok((present, value.take()))
            })?;
            iter.symbols.pop_front();
            if !present {
                continue;
            }
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
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}
