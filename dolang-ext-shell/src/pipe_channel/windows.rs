#![allow(dead_code)]

use std::{fmt, marker::PhantomData};

use dolang::runtime::{
    Error, Instance, Object, Result, Slot, State, Strand, Value, Vm, vm::Builder,
};

use crate::global::Global;

pub(crate) struct PipeReceiver;
pub(crate) struct PipeSender;

pub(crate) struct RecvGuard(PhantomData<()>);
pub(crate) struct SendGuard(PhantomData<()>);

impl RecvGuard {
    pub(crate) fn fd<'v, 's>(&self) -> Result<'v, 's, std::convert::Infallible> {
        unreachable!("windows pipe guard should never be reached")
    }
}

impl SendGuard {
    pub(crate) fn fd<'v, 's>(&self) -> Result<'v, 's, std::convert::Infallible> {
        unreachable!("windows pipe guard should never be reached")
    }
}

pub(crate) fn install<'v>(_builder: &mut Builder<'v>) {}

pub(crate) fn make_pair<'v>(_vm: &Vm<'v>, _out_send: Slot<'v, '_>, _out_recv: Slot<'v, '_>) {
    unreachable!("windows pipe constructor should never be installed")
}

pub(crate) async fn negotiate_recv<'v, 's>(
    input: &Value<'v>,
    _strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, Option<RecvGuard>> {
    let _ = global.types.pipe_receiver.downcast(input);
    Ok(None)
}

pub(crate) async fn negotiate_send<'v, 's>(
    output: &Value<'v>,
    _strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, Option<SendGuard>> {
    let _ = global.types.pipe_sender.downcast(output);
    Ok(None)
}

impl<'v> Object<'v> for PipeReceiver {
    const NAME: &'v str = "PipeReceiver";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<proc.PipeReceiver>").map_err(|e| Error::runtime(strand, e.to_string()))
    }
}

impl<'v> Object<'v> for PipeSender {
    const NAME: &'v str = "PipeSender";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<proc.PipeSender>").map_err(|e| Error::runtime(strand, e.to_string()))
    }
}
