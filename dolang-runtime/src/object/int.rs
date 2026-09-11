use std::{hash::DefaultHasher, num::IntErrorKind, ops::ControlFlow};

use dolang_util::alias;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{Inspect, Protocol, Recv, dispatch_native_method, members},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        Output, Slot, Value,
        fmt::{self, Format, Spec},
        prim::Prim,
    },
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
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt::format_int(*this.get(), strand, spec, w)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().int)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", *this.get())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::int_get(strand, &this, field, _out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::int_mcall(strand, &this, *this.get(), method, _args, _out).await
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
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        use crate::value::fmt::{Fill, Kind, Pad};

        let kind = spec.kind.ok_or_else(|| fmt::unresolved_kind(strand))?;
        if !kind.is_text() || spec.sign.is_some() || spec.fill == Fill::Zero {
            return fmt::format_int(this.get().value, strand, spec, w);
        }
        if spec.alt {
            return Err(Error::type_error(
                strand,
                "unsupported integer format option",
            ));
        }
        let mut pad = Pad::new(*spec, w);
        match kind {
            Kind::Str => Self::op_display(this, strand, &mut pad)?,
            Kind::Dbg => Self::op_debug(this, strand, &mut pad)?,
            Kind::Verbatim => Self::op_verbatim(this, strand, &mut pad)?,
            _ => unreachable!(),
        }
        pad.finish(strand)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().int)
    }

    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.get().text)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.get().value)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        fmt!(strand, w, "{}", borrow.text)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::int_get(strand, &this, field, _out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::int_mcall(strand, &this, this.get().value, method, _args, _out).await
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

pub(crate) fn coerce<'v, 's>(
    value: &Value<'v>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, i128> {
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

fn construct<'v, 's>(value: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, i128> {
    match value.to_prim(strand) {
        Ok(Prim::Int(value)) => Ok(value),
        Ok(Prim::Bool(value)) => Ok(value as i128),
        Ok(Prim::F64(value))
            if value.is_finite()
                && value.fract() == 0.0
                && value >= i128::MIN as f64
                && value < -(i128::MIN as f64) =>
        {
            Ok(value as i128)
        }
        _ => Err(Error::type_error(
            strand,
            "Int: expected Int, Bool, or integral Float",
        )),
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
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<type std.Int>")
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().num)
            || supertype.eq(strand, crate::value::TypeObject::Value)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
                Getter(sym::MIN_CONST),
                Getter(sym::MAX_CONST),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::ADD_METHOD),
                Method(sym::SUB_METHOD),
                Method(sym::RSUB_METHOD),
                Method(sym::MUL_METHOD),
                Method(sym::DIV_METHOD),
                Method(sym::RDIV_METHOD),
                Method(sym::EDIV_METHOD),
                Method(sym::REDIV_METHOD),
                Method(sym::MOD_METHOD),
                Method(sym::RMOD_METHOD),
                Method(sym::BAND_METHOD),
                Method(sym::BOR_METHOD),
                Method(sym::BXOR_METHOD),
                Method(sym::SHL_METHOD),
                Method(sym::SHR_METHOD),
                Method(sym::NEG_METHOD),
                Method(sym::BNOT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::BOOL_METHOD),
                Method(sym::HASH_METHOD),
                Method(sym::ABS),
                Method(sym::SIGNUM),
                Method(sym::ROUND),
                Method(sym::FLOOR),
                Method(sym::CEIL),
                Method(sym::TRUNC),
                Method(sym::MIN),
                Method(sym::MAX),
                Method(sym::CLAMP),
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
            sym::MIN_CONST => {
                Output::set(strand, out, i128::MIN);
                return Ok(());
            }
            sym::MAX_CONST => {
                Output::set(strand, out, i128::MAX);
                return Ok(());
            }
            _ => {}
        }
        match field.tag() {
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
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
            sym::ABS
            | sym::SIGNUM
            | sym::ROUND
            | sym::FLOOR
            | sym::CEIL
            | sym::TRUNC
            | sym::MIN
            | sym::MAX
            | sym::CLAMP => {
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
        let coerced = construct(&value, strand)?;
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
                let coerced = construct(&value, strand)?;
                let native = Value::from_int(strand, coerced);
                self_val.op_fill(strand, &strand.singletons().int, native)?;
                Ok(())
            }
            _ => dispatch_native_method(strand, &strand.singletons().int, method, args, out).await,
        }
    }
}
