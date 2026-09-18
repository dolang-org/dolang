use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::{self, Sym},
    value::{Output, Slot, Value},
    vm::Vm,
};

use super::{
    protocol::{Protocol, Recv, Spread, SpreadContext},
    record::ArgItem,
    tuple,
};

pub(crate) struct ArgPack<'v> {
    inner: Vec<ArgItem<'v>>,
}

impl<'v> ArgPack<'v> {
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

unsafe impl<'v> Collect for ArgPack<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
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

impl<'v> Protocol<'v> for ArgPack<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().args)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<args>")
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut int = 0i64;
        let pack = this.borrow(strand)?;
        for (key, value) in &pack.inner {
            let mut value = value.dup();
            if context == SpreadContext::Sequence {
                if let Some(key) = key {
                    value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_object(key.clone()), value.take()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                } else {
                    value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_i64(strand, int), value.take()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                    int += 1;
                }
            } else {
                if let Some(key) = key {
                    let mut key = Value::from_object(key.clone());
                    sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
                } else {
                    sink.positional(strand, Slot::new(&mut value))?;
                }
            }
        }
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
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
            _ => Err(Error::field(strand, method)),
        }
    }
}
