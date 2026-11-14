use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    error::Result,
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::{self, Sym},
    value::{Output, Slot, Value},
    vm::Vm,
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

    pub(crate) fn from_args(vm: &Vm<'v>, args: Args<'v, '_>) -> Self {
        let mut inner = Vec::new();
        for arg in args {
            match arg {
                Arg::Pos(mut slot) => inner.push((None, slot.take())),
                Arg::Key(sym, mut slot) => inner.push((Some(vm.sym_obj(sym)), slot.take())),
            }
        }
        Self::new(inner)
    }

    pub(crate) fn push(&mut self, item: ArgItem<'v>) {
        self.inner.push(item);
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
                    let key = Value::from_object(unsafe { key.dup() });
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
                        Arg::Pos(mut value) => pack.push((None, value.take())),
                        Arg::Key(key, mut value) => {
                            pack.push((Some(strand.sym_obj(key)), value.take()));
                        }
                    }
                }
                Ok(())
            }
            _ => instance_mcall_fallback(strand, &this, method, args, out),
        }
    }
}
