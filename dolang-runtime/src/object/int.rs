use std::{fmt, hash::DefaultHasher, num::IntErrorKind, ops::ControlFlow};

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

unsafe impl Collect for i128 {
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
    left: i128,
    right: &Value<'v>,
    op: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
) -> Result<'v, 's, Value<'v>> {
    let prim = right.to_prim(strand)?;
    let value = op(&Prim::from(left), strand, &prim)?;
    Ok(Value::from_prim(strand, value))
}

fn rbinop<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: i128,
    right: &Value<'v>,
    op: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
) -> Result<'v, 's, Value<'v>> {
    let prim = right.to_prim(strand)?;
    let value = op(&prim, strand, &Prim::from(left))?;
    Ok(Value::from_prim(strand, value))
}

impl<'v> Protocol<'v> for i128 {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().int.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", *this.get()).into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        *this.get() != 0
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
        this.get()
            .checked_neg()
            .ok_or_else(|| Error::overflow(strand))
            .map(|v| Value::from_int(strand, v))
    }

    fn op_bnot<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_int(strand, !*this.get()))
    }

    fn op_band<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_band)
    }

    fn op_bor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_bor)
    }

    fn op_bxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_bxor)
    }

    fn op_shl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_shl)
    }

    fn op_shr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, *this.get(), other, Prim::op_shr)
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
}

pub(crate) struct Verbatim {
    pub(crate) value: i128,
    text: alias::Box<str>,
}

impl Verbatim {
    pub(crate) fn new(value: i128, text: &str) -> Self {
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
        out.store(strand.singletons().int.dup())
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
        let borrow = this.get();
        write!(w, "{}", borrow.text).into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        this.get().value != 0
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
        this.get()
            .value
            .checked_neg()
            .ok_or_else(|| Error::overflow(strand))
            .map(|v| Value::from_int(strand, v))
    }

    fn op_bnot<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_int(strand, !this.get().value))
    }

    fn op_band<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_band)
    }

    fn op_bor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_bor)
    }

    fn op_bxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_bxor)
    }

    fn op_shl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_shl)
    }

    fn op_shr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        binop(strand, this.get().value, other, Prim::op_shr)
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

fn coerce_to_int<'v, 's>(value: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, i128> {
    if let Some(str) = value.as_str_raw(strand) {
        str.parse::<i128>().map_err(|e| match e.kind() {
            IntErrorKind::Zero => unreachable!(),
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => Error::overflow(strand),
            IntErrorKind::Empty | IntErrorKind::InvalidDigit | _ => {
                Error::type_error(strand, format!("int: not a valid integer: {:?}", str))
            }
        })
    } else {
        match value.to_prim(strand)? {
            Prim::Int(v) => Ok(v),
            Prim::F64(v) => Ok(v as i128),
            Prim::Bool(v) => Ok(v as i128),
            Prim::Nil => Err(Error::type_error(strand, "int: `nil` can't be converted")),
        }
    }
}

pub(crate) struct Int;

unsafe impl Collect for Int {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Int {
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
        write!(w, "<type std.int>").into_do(strand)
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
                Sym::well_known(sym::BAND_METHOD),
                Sym::well_known(sym::BOR_METHOD),
                Sym::well_known(sym::BXOR_METHOD),
                Sym::well_known(sym::SHL_METHOD),
                Sym::well_known(sym::SHR_METHOD),
                Sym::well_known(sym::NEG_METHOD),
                Sym::well_known(sym::BNOT_METHOD),
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
            | sym::BAND_METHOD
            | sym::BOR_METHOD
            | sym::BXOR_METHOD
            | sym::SHL_METHOD
            | sym::SHR_METHOD
            | sym::NEG_METHOD
            | sym::BNOT_METHOD
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

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], _) = unpack!(strand, args, 1, 0)?;
        let coerced = coerce_to_int(&value, strand)?;
        Output::set(strand, out, coerced);
        Ok(())
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
                let coerced = coerce_to_int(&value, strand)?;
                let native = Value::from_int(strand, coerced);
                self_val.op_fill(strand, &strand.vm().singletons().int, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.vm().singletons().int, method, args, out)
                    .await
            }
        }
    }
}
