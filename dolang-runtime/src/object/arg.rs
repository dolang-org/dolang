use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    error::Result,
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::{self, Sym},
    value::{Output, Slot, Value},
    with_slots,
};

use super::{
    pack::ArgItem,
    protocol::{Protocol, Recv, Spread, SpreadContext, instance_mcall_fallback},
};

pub(crate) struct ArgAcc<'v> {
    inner: Vec<ArgItem<'v>>,
}

impl<'v> ArgAcc<'v> {
    pub(crate) fn new(inner: Vec<ArgItem<'v>>) -> Self {
        Self { inner }
    }

    pub(crate) fn push(&mut self, item: ArgItem<'v>) {
        self.inner.push(item);
    }

    pub(crate) fn items(&self) -> &[ArgItem<'v>] {
        &self.inner
    }

    /// Removes and returns every item.
    pub(crate) fn take(&mut self) -> Vec<ArgItem<'v>> {
        std::mem::take(&mut self.inner)
    }
}

unsafe impl<'v> Collect for ArgAcc<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.inner {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.inner.clear()
    }
}

impl<'v> Protocol<'v> for ArgAcc<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().arg_acc)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<arg accumulator>")
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let pack = this.borrow(strand)?;
        with_slots!(strand, |mut key_slot, mut value_slot| {
            for (key, value) in &pack.inner {
                Output::set(strand, &mut value_slot, value);
                if let Some(key) = key {
                    // SAFETY: the reference moves straight into `key_slot`
                    let key = unsafe { Value::from_object(key.dup()) };
                    Output::set(strand, &mut key_slot, key);
                    sink.keyed(
                        strand,
                        Slot::reborrow(&mut key_slot),
                        Slot::reborrow(&mut value_slot),
                    )?;
                } else {
                    sink.positional(strand, Slot::reborrow(&mut value_slot))?;
                }
            }
            Ok(())
        })
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::PUSH => {
                let mut pack = this.borrow_mut(strand)?;
                for arg in args {
                    match arg {
                        // SAFETY: the value moves straight into the borrowed pack
                        Arg::Pos(mut value) => pack.push((None, unsafe { value.take(strand) })),
                        Arg::Key(key, mut value) => {
                            // SAFETY: the symbol and the value move straight into the
                            // borrowed pack
                            pack.push((Some(unsafe { strand.sym_obj(key) }), unsafe {
                                value.take(strand)
                            }));
                        }
                    }
                }
                Ok(())
            }
            _ => instance_mcall_fallback(strand, &this, method, args, out),
        }
    }
}
