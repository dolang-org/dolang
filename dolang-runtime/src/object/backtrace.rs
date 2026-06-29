use std::{fmt, ops::ControlFlow};

use crate::{
    error::{BacktraceIter, Error, Result, ResultExt, UnwindEntry},
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::{self, Sym},
    value::{Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::protocol::{Inspect, Protocol, Recv};

pub(crate) fn create<'v>(
    strand: &mut Strand<'v, '_>,
    entries: Vec<UnwindEntry<'v>>,
    out: impl Output<'v>,
) {
    strand.builtin_types().backtrace.create(
        strand,
        Backtrace {
            entries: entries.into_boxed_slice(),
        },
        out,
    );
}

pub(crate) fn entries_from_value<'v>(
    vm: &Vm<'v>,
    value: &Value<'v>,
) -> Option<Vec<UnwindEntry<'v>>> {
    let backtrace = value.downcast_ref(vm.builtin_types().backtrace)?;
    Some(backtrace.get().entries.to_vec())
}

pub(crate) fn iter_from_value<'v, 'a>(
    vm: &'a Vm<'v>,
    value: &'a Value<'v>,
) -> Option<BacktraceIter<'v, 'a>> {
    let backtrace = value.downcast_ref(vm.builtin_types().backtrace)?;
    Some(BacktraceIter::new(vm, backtrace.get().entries.iter()))
}

pub(crate) struct Backtrace<'v> {
    entries: Box<[UnwindEntry<'v>]>,
}

unsafe impl<'v> Collect for Backtrace<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Backtrace<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.singletons().backtrace)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().backtrace)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<backtrace {}>", this.get().entries.len()).into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![Sym::well_known(sym::LEN), Sym::well_known(sym::ITER_METHOD)],
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                Output::set(strand, out, this.get().entries.len());
                Ok(())
            }
            sym::ITER_METHOD => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.builtin_types().backtrace_iter.create(
            strand,
            Iter {
                entries: this.get().entries.to_vec().into_boxed_slice(),
                index: 0,
            },
            out,
        );
        Ok(())
    }
}

pub(crate) struct Iter<'v> {
    entries: Box<[UnwindEntry<'v>]>,
    index: usize,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.entries = Vec::new().into_boxed_slice();
        self.index = 0;
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        write!(
            w,
            "<backtrace.iter {}/{}>",
            borrow.index,
            borrow.entries.len()
        )
        .into_do(strand)
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let Some(entry) = borrow.entries.get(borrow.index).cloned() else {
            return Ok(false);
        };
        borrow.index += 1;
        drop(borrow);
        strand
            .vm()
            .builtin_types()
            .backtrace_frame
            .create(strand, Frame { entry }, out);
        Ok(true)
    }
}

pub(crate) struct Frame<'v> {
    entry: UnwindEntry<'v>,
}

unsafe impl<'v> Collect for Frame<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Frame<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().value)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(
            w,
            "<backtrace frame {}.{}>",
            this.get().entry.module(strand),
            this.get().entry.receiver(strand)
        )
        .into_do(strand)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::MODULE => {
                let module = this.get().entry.module(strand);
                Output::set(strand, out, module.as_ref());
                Ok(())
            }
            sym::RECEIVER => {
                let receiver = this.get().entry.receiver(strand);
                Output::set(strand, out, receiver.as_ref());
                Ok(())
            }
            sym::METHOD => {
                if let Some(method) = this.get().entry.method() {
                    Output::set(strand, out, method.as_ref());
                } else {
                    out.store(Value::NIL);
                }
                Ok(())
            }
            sym::SOURCE => {
                if let Some((source, _)) = this.get().entry.source(strand) {
                    Output::set(strand, out, source.as_ref());
                } else {
                    out.store(Value::NIL);
                }
                Ok(())
            }
            sym::LINE => {
                if let Some((_, line)) = this.get().entry.source(strand) {
                    Output::set(strand, out, line);
                } else {
                    out.store(Value::NIL);
                }
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }
}

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.strand.Backtrace>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: Vec::new(),
        })
    }
}
