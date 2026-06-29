use std::{
    fmt,
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::Tag,
    unpack,
    value::{Output, Slot, Value},
};

use super::protocol::{Protocol, Recv};

pub(crate) struct SymObj {
    pub(crate) tag: Tag,
    pub(crate) name: String,
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

    fn op_display_arg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, ":{}:", this.get().name).into_do(strand)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.get().name).into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<sym {}>", this.get().name).into_do(strand)
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.sym>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Constructor: sym(value) - convert to symbol
        let ([value], _) = unpack!(strand, args, 1, 0)?;
        if value.as_sym(strand).is_some() {
            Output::set(strand, out, value);
            Ok(())
        } else if let Some(str) = value.as_str_raw(strand) {
            // FIXME: don't do this every time, move it to an interrupt
            strand.sym_gc();
            out.store(Value::from_object(strand.sym_register_obj(str)));
            Ok(())
        } else {
            Err(Error::type_error(strand, "sym: not a string"))
        }
    }
}
