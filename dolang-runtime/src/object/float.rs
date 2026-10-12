use std::{hash::DefaultHasher, ops::ControlFlow};

use dolang_util::alias;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{Inspect, Protocol, Recv, instance_get_fallback, members, type_mcall_fallback},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        Output, Slot, Value,
        fmt::{self, Fill, Format, Kind, Pad, Spec},
        prim::Prim,
    },
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
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::float_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::float_mcall(strand, &this, *this.get(), method, args, out).await
    }

    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt::format_float(*this.get(), strand, spec, w)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", *this.get())
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(*this.get() != 0.0)
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

    fn op_radd<'a, 's>(
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

    fn op_rmul<'a, 's>(
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
        let prim = other
            .to_prim(strand)
            .map_err(|_| Error::not_supported(strand))?;
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
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().float)
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
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::float_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::num::float_mcall(strand, &this, this.get().value, method, args, out).await
    }

    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let kind = spec.kind.ok_or_else(|| fmt::unresolved_kind(strand))?;
        if !kind.is_text()
            || spec.sign.is_some()
            || spec.precision.is_some()
            || spec.fill == Fill::Zero
        {
            return fmt::format_float(this.get().value, strand, spec, w);
        }
        if spec.alt {
            return Err(Error::type_error(strand, "unsupported float format option"));
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
        Output::set(strand, out, &strand.singletons().float)
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
        fmt!(strand, w, "{:?}", this.get().value)
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(this.get().value != 0.0)
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

    fn op_radd<'a, 's>(
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

    fn op_rmul<'a, 's>(
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
        let prim = other
            .to_prim(strand)
            .map_err(|_| Error::not_supported(strand))?;
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
) -> Result<'v, 's, f64> {
    if let Some(str) = value.as_str_raw(strand) {
        str.parse::<f64>()
            .map_err(|_| Error::value(strand, format!("float: not a valid float: {:?}", str)))
    } else {
        match value.to_prim(strand)? {
            Prim::Int(v) => Ok(v as f64),
            Prim::F64(v) => Ok(v),
            Prim::Bool(v) => Ok(v as i32 as f64),
            Prim::Nil => Err(Error::type_error(strand, "float: `nil` can't be converted")),
        }
    }
}

fn construct<'v, 's>(value: &Value<'v>, strand: &mut Strand<'v, 's>) -> Result<'v, 's, f64> {
    match value.to_prim(strand) {
        Ok(Prim::F64(value)) => Ok(value),
        Ok(Prim::Int(value)) => {
            let converted = value as f64;
            if converted.is_finite()
                && converted >= i128::MIN as f64
                && converted < -(i128::MIN as f64)
                && converted as i128 == value
            {
                Ok(converted)
            } else {
                Err(Error::type_error(
                    strand,
                    "Float: Int cannot be represented exactly",
                ))
            }
        }
        _ => Err(Error::type_error(strand, "Float: expected Float or Int")),
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
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<type std.Float>")
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().num)
                .unwrap_or(false)
            || supertype
                .eq(strand, crate::value::TypeObject::Value)
                .unwrap_or(false)
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

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
                Getter(sym::EPSILON_CONST),
                Getter(sym::INF_CONST),
                Getter(sym::NAN_CONST),
            ],
            members: members![
                Method(sym::INIT_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::ADD_METHOD),
                Method(sym::RADD_METHOD),
                Method(sym::SUB_METHOD),
                Method(sym::RSUB_METHOD),
                Method(sym::MUL_METHOD),
                Method(sym::RMUL_METHOD),
                Method(sym::DIV_METHOD),
                Method(sym::RDIV_METHOD),
                Method(sym::EDIV_METHOD),
                Method(sym::REDIV_METHOD),
                Method(sym::MOD_METHOD),
                Method(sym::RMOD_METHOD),
                Method(sym::NEG_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::REQ_METHOD),
                Method(sym::NE_METHOD),
                Method(sym::RNE_METHOD),
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
                Method(sym::IS_NAN),
                Method(sym::IS_FINITE),
                Method(sym::IS_INFINITE),
                Method(sym::IS_NORMAL),
                Method(sym::IS_SUBNORMAL),
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
            sym::EPSILON_CONST => {
                Output::set(strand, out, f64::EPSILON);
                return Ok(());
            }
            sym::INF_CONST => {
                Output::set(strand, out, f64::INFINITY);
                return Ok(());
            }
            sym::NAN_CONST => {
                Output::set(strand, out, f64::NAN);
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
            | sym::RADD_METHOD
            | sym::SUB_METHOD
            | sym::RSUB_METHOD
            | sym::MUL_METHOD
            | sym::RMUL_METHOD
            | sym::DIV_METHOD
            | sym::RDIV_METHOD
            | sym::EDIV_METHOD
            | sym::REDIV_METHOD
            | sym::MOD_METHOD
            | sym::RMOD_METHOD
            | sym::NEG_METHOD
            | sym::EQ_METHOD
            | sym::REQ_METHOD
            | sym::NE_METHOD
            | sym::RNE_METHOD
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
            | sym::CLAMP
            | sym::IS_NAN
            | sym::IS_FINITE
            | sym::IS_INFINITE
            | sym::IS_NORMAL
            | sym::IS_SUBNORMAL => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
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
                let coerced = construct(&value, strand)?;
                let native = Value::from_f64(strand, coerced);
                self_val.op_fill(strand, &strand.singletons().float, native)?;
                Ok(())
            }
            _ => type_mcall_fallback(strand, &strand.singletons().float, method, args, out).await,
        }
    }
}
