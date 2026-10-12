use std::{
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::Tag,
    unpack,
    value::{Output, Slot, Value},
    vm::Vm,
};

use super::protocol::{Inspect, Protocol, Recv};

pub(crate) struct SymObj {
    pub(crate) tag: Tag,
    pub(crate) name: String,
    pub(crate) private: bool,
}

unsafe impl Collect for SymObj {
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

impl<'v> Protocol<'v> for SymObj {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().sym)
    }

    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, ":{}:", this.get().name)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", this.get().name)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<sym {}>", this.get().name)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(osym) = other.downcast_ref(strand.builtin_types().sym) {
            Ok(Value::from_bool(osym.get().tag == this.get().tag))
        } else {
            Ok(Value::from_bool(false))
        }
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.get().tag.hash(hasher);
        Ok(())
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(osym) = other.downcast_ref(strand.builtin_types().sym) {
            Ok(Value::from_bool(this.receiver.get().name < osym.get().name))
        } else {
            Err(Error::not_supported(strand))
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Sym>")
    }

    // Final, so no class inherits these members. The lists stay empty until
    // inspection serves something besides inheritance.
    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: true,
            members: &[],
            type_members: &[],
        })
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        value
            .as_sym(strand)
            .ok_or_else(|| Error::type_error(strand, "Sym: expected Sym"))?;
        Output::set(strand, out, value);
        Ok(())
    }
}
