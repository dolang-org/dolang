use std::{
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit},
    strand::Strand,
    sym::Tag,
    unpack,
    value::{Slot, Value},
    vm::{AllocExt, Vm},
};

use super::protocol::{GcObj, Inspect, Protocol, Recv, members};

pub(crate) struct SymData {
    pub(crate) tag: Tag,
    pub(crate) name: String,
    pub(crate) private: bool,
}

unsafe impl gc::ImmState for SymData {
    const CYCLIC: bool = false;
}

impl gc::Annex for SymData {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

/// A symbol object.
pub(crate) type SymObj = gc::Imm<SymData>;

impl<'v> GcObj<'v, SymObj> {
    pub(crate) fn tag(&self) -> Tag {
        self.read(|p| p.get().tag)
    }

    /// The symbol's name, which lives out of line.
    pub(crate) fn name(&self) -> &str {
        self.project(|p| &p.get().name)
    }
}

impl<'v> Protocol<'v> for SymObj {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Sym";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().sym)
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
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if let Some(osym) = other.downcast_ref(strand.builtin_types().sym) {
            strand.store(&mut out, Value::from_bool(osym.tag() == this.get().tag));
            Ok(())
        } else {
            Err(Error::unsupported(strand, "`==`"))
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
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if let Some(osym) = other.downcast_ref(strand.builtin_types().sym) {
            strand.store(
                &mut out,
                Value::from_bool(this.get().name.as_str() < osym.name()),
            );
            Ok(())
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }
}

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Sym";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    // Final, so no class inherits these members. The lists stay empty until
    // inspection serves something besides inheritance.
    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: true,
            members: members![..Value],
            type_members: &[],
        })
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => value);
        value
            .as_sym(strand)
            .ok_or_else(|| Error::type_error(strand, "Sym: expected Sym"))?;
        strand.store(out, value);
        Ok(())
    }
}
