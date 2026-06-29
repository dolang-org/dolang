use std::{fmt, hash::DefaultHasher, ops::ControlFlow};

use dolang_util::alias;

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{Inspect, Protocol, Recv, dispatch_native_method},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Value, prim::Prim},
    vm::Vm,
};

unsafe impl Collect for f64 {
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

fn binop<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: f64,
    right: &Value<'v>,
    op: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
) -> Result<'v, 's, Value<'v>> {
    let prim = right.to_prim(strand)?;
    let value = op(&Prim::from(left), strand, &prim)?;
    Ok(Value::from_prim(strand, value))
}

fn rbinop<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: f64,
    right: &Value<'v>,
    op: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
) -> Result<'v, 's, Value<'v>> {
    let prim = right.to_prim(strand)?;
    let value = op(&prim, strand, &Prim::from(left))?;
    Ok(Value::from_prim(strand, value))
}

impl<'v> Protocol<'v> for f64 {
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", *this.get()).into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        *this.get() != 0.0
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        Prim::from(*this.get()).op_hash(strand, hasher);
        Ok(())
    }

    fn op_neg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_f64(strand, -*this.get()))
    }

    fn op_add<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_add)
    }

    fn op_sub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_sub)
    }

    fn op_rsub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, *this.get(), other, Prim::op_sub)
    }

    fn op_mul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_mul)
    }

    fn op_div<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_div)
    }

    fn op_rdiv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, *this.get(), other, Prim::op_div)
    }

    fn op_ediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_ediv)
    }

    fn op_rediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, *this.get(), other, Prim::op_ediv)
    }

    fn op_mod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_mod)
    }

    fn op_rmod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, *this.get(), other, Prim::op_mod)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let prim = other.to_prim(strand)?;
        Ok(Value::from_bool(
            Prim::from(*this.get()).op_eq(strand, &prim),
        ))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_lt)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().float.dup())
    }
}

pub(crate) struct Verbatim {
    pub(crate) value: f64,
    text: alias::Box<str>,
}

impl Verbatim {
    pub(crate) fn new(value: f64, text: &str) -> Self {
        Self {
            value,
            text: text.into(),
        }
    }
}

unsafe impl Collect for Verbatim {
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

impl<'v> Protocol<'v> for Verbatim {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().float.dup())
    }

    fn op_display_arg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.get().text).into_do(strand)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.get().value).into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{:?}", this.get().value).into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        this.get().value != 0.0
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        Prim::from(this.get().value).op_hash(strand, hasher);
        Ok(())
    }

    fn op_neg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_f64(strand, -this.get().value))
    }

    fn op_add<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_add)
    }

    fn op_sub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_sub)
    }

    fn op_rsub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, this.get().value, other, Prim::op_sub)
    }

    fn op_mul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_mul)
    }

    fn op_div<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_div)
    }

    fn op_rdiv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, this.get().value, other, Prim::op_div)
    }

    fn op_ediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_ediv)
    }

    fn op_rediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, this.get().value, other, Prim::op_ediv)
    }

    fn op_mod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_mod)
    }

    fn op_rmod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        rbinop(strand, this.get().value, other, Prim::op_mod)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let prim = other.to_prim(strand)?;
        Ok(Value::from_bool(
            Prim::from(this.get().value).op_eq(strand, &prim),
        ))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_lt)
    }
}

fn coerce_to_f64<'v, 's>(value: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, f64> {
    if let Some(str) = value.as_str_raw(strand) {
        str.parse::<f64>()
            .map_err(|_| Error::type_error(strand, format!("float: not a valid float: {:?}", str)))
    } else {
        match value.to_prim(strand)? {
            Prim::Int(v) => Ok(v as f64),
            Prim::F64(v) => Ok(v),
            Prim::Bool(v) => Ok(v as i32 as f64),
            Prim::Nil => Err(Error::type_error(strand, "float: `nil` can't be converted")),
        }
    }
}

pub(crate) struct Float;

unsafe impl Collect for Float {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Float {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.float>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], _) = unpack!(strand, args, 1, 0)?;
        let coerced = coerce_to_f64(&value, strand)?;
        Output::set(strand, out, coerced);
        Ok(())
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![
                Sym::well_known(sym::STR_METHOD),
                Sym::well_known(sym::DBG_METHOD),
                Sym::well_known(sym::ADD_METHOD),
                Sym::well_known(sym::SUB_METHOD),
                Sym::well_known(sym::RSUB_METHOD),
                Sym::well_known(sym::MUL_METHOD),
                Sym::well_known(sym::DIV_METHOD),
                Sym::well_known(sym::RDIV_METHOD),
                Sym::well_known(sym::EDIV_METHOD),
                Sym::well_known(sym::REDIV_METHOD),
                Sym::well_known(sym::MOD_METHOD),
                Sym::well_known(sym::RMOD_METHOD),
                Sym::well_known(sym::NEG_METHOD),
                Sym::well_known(sym::EQ_METHOD),
                Sym::well_known(sym::LT_METHOD),
                Sym::well_known(sym::BOOL_METHOD),
                Sym::well_known(sym::HASH_METHOD),
            ],
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::ADD_METHOD
            | sym::SUB_METHOD
            | sym::RSUB_METHOD
            | sym::MUL_METHOD
            | sym::DIV_METHOD
            | sym::RDIV_METHOD
            | sym::EDIV_METHOD
            | sym::REDIV_METHOD
            | sym::MOD_METHOD
            | sym::RMOD_METHOD
            | sym::NEG_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val, value], []) = unpack!(strand, args, 2, 0)?;
                let coerced = coerce_to_f64(&value, strand)?;
                let native = Value::from_f64(strand, coerced);
                self_val.op_fill(strand, &strand.vm().singletons().float, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.vm().singletons().float, method, args, out)
                    .await
            }
        }
    }
}
