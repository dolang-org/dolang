use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{BacktraceIter, Error, Result, UnwindEntry},
    gc::{Collect, arena::Visit},
    object::{iter, protocol::members},
    strand::Strand,
    sym::{self, Sym},
    unpack,
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
    Some(BacktraceIter::new(backtrace.get().entries.iter()))
}

pub(crate) struct Backtrace<'v> {
    entries: Box<[UnwindEntry<'v>]>,
}

unsafe impl<'v> Collect for Backtrace<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for entry in &self.entries {
            entry.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Backtrace<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().backtrace)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<backtrace>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: members![
                Getter(sym::LEN),
                Method(sym::ITER_METHOD),
                Method(sym::FMT_METHOD)
            ],
            type_members: &[],
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
            _ => iter::iterable_get(strand, &this, field, out),
        }
    }

    /// Dispatch a method call on a backtrace.
    ///
    /// Required rather than optional: [`iter::iterable_get`] hands back a
    /// `BoundMethod` that re-enters `op_mcall`, so without an explicit
    /// implementation the default (`op_get` then `op_call`) recurses.
    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::LEN => Err(Error::type_error(
                strand,
                "backtrace.len is a field, not a method",
            )),
            sym::ITER_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Self::op_iter(this, strand, out).await
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
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

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for entry in &self.entries[self.index..] {
            entry.accept(visit)?;
        }
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
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<backtrace.iter>")
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

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }
}

pub(crate) struct Frame<'v> {
    entry: UnwindEntry<'v>,
}

unsafe impl<'v> Collect for Frame<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.entry.accept(visit)
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
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<backtrace frame>")
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::MODULE => {
                let module = this.get().entry.module();
                Output::set(strand, out, module.as_ref());
                Ok(())
            }
            sym::RECEIVER => {
                let receiver = this.get().entry.receiver();
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
                if let Some((source, _)) = this.get().entry.source() {
                    Output::set(strand, out, source.as_ref());
                } else {
                    out.store(Value::NIL);
                }
                Ok(())
            }
            sym::LINE => {
                if let Some((_, line)) = this.get().entry.source() {
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

    /// Mirrors the instance-level `op_subtype`.
    ///
    /// `is_instance_of` dispatches on the type object, not the instance, so
    /// without this `type bt Iterable` would answer `false` even though a
    /// backtrace both follows the protocol and exposes the surface.
    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type strand.Backtrace>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: &[],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
            ],
        })
    }
}
