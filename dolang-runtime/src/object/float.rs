use std::{hash::DefaultHasher, ops::ControlFlow};

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

pub(crate) fn coerce<'v, 's>(
    value: &Value<'v>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, f64> {
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
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().num)
            || supertype.eq(strand, crate::value::TypeObject::Value)
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
                Method(sym::NEG_METHOD),
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
                let coerced = construct(&value, strand)?;
                let native = Value::from_f64(strand, coerced);
                self_val.op_fill(strand, &strand.singletons().float, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.singletons().float, method, args, out).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{call, error::ErrorKind, method, test_support::with_vm, value::prim::Prim};

    use super::*;

    fn make_float<'v>(strand: &mut Strand<'v, '_>, value: f64, out: Slot<'v, '_>) {
        strand.builtin_types().f64.create(strand, value, out);
    }

    fn make_verbatim<'v>(strand: &mut Strand<'v, '_>, value: f64, text: &str, out: Slot<'v, '_>) {
        strand
            .builtin_types()
            .verbatim_f64
            .create(strand, Verbatim::new(value, text), out);
    }

    #[test]
    fn f64_op_debug_bool_and_neg() {
        with_vm(async |strand, [mut slot]| {
            make_float(strand, 2.5, Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut s = String::new();
                    f64::op_debug(recv.clone(), strand, &mut s).unwrap();
                    assert_eq!(s, "2.5");
                    assert!(f64::op_bool(recv.clone(), strand));
                    let neg = f64::op_neg(recv, strand).unwrap();
                    assert_eq!(neg.to_prim(strand).unwrap(), Prim::F64(-2.5));
                });

            make_float(strand, 0.0, Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(!f64::op_bool(recv, strand));
                });
        });
    }

    #[test]
    fn f64_op_add_and_op_rsub_promote_mixed_arithmetic_to_float() {
        with_vm(async |strand, [mut slot, mut other_slot]| {
            make_float(strand, 2.5, Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            Output::set(strand, &mut other_slot, 3_i64);
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let sum = f64::op_add(recv, strand, &other_slot).unwrap();
                    assert_eq!(sum.to_prim(strand).unwrap(), Prim::F64(5.5));
                });

            // `10 - 2.5` via the receiver's `op_rsub` (right-hand subtraction).
            Output::set(strand, &mut other_slot, 10_i64);
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let diff = f64::op_rsub(recv, strand, &other_slot).unwrap();
                    assert_eq!(diff.to_prim(strand).unwrap(), Prim::F64(7.5));
                });
        });
    }

    #[test]
    fn f64_op_eq_and_op_lt() {
        with_vm(
            async |strand, [mut slot, mut equal_slot, mut greater_slot]| {
                make_float(strand, 2.5, Slot::reborrow(&mut slot));
                let value: &Value = &slot;
                Output::set(strand, &mut equal_slot, 2.5_f64);
                Output::set(strand, &mut greater_slot, 5.0_f64);
                strand.builtin_types().f64.cast(value).unwrap().enter_sync(
                    strand,
                    |strand, recv| {
                        assert!(
                            f64::op_eq(recv, strand, &equal_slot)
                                .unwrap()
                                .to_bool(strand)
                        );
                    },
                );
                strand.builtin_types().f64.cast(value).unwrap().enter_sync(
                    strand,
                    |strand, recv| {
                        assert!(
                            f64::op_lt(recv, strand, &greater_slot)
                                .unwrap()
                                .to_bool(strand)
                        );
                    },
                );
            },
        );
    }

    #[test]
    fn f64_remaining_arithmetic_ops_delegate_to_prim() {
        with_vm(async |strand, [mut slot, mut other_slot]| {
            Output::set(strand, &mut other_slot, 2.0_f64);

            macro_rules! check {
                ($op:ident, $expected:expr) => {{
                    make_float(strand, 9.0, Slot::reborrow(&mut slot));
                    let value: &Value = &slot;
                    strand.builtin_types().f64.cast(value).unwrap().enter_sync(
                        strand,
                        |strand, recv| {
                            let result = f64::$op(recv, strand, &other_slot).unwrap();
                            assert_eq!(result.to_prim(strand).unwrap(), $expected);
                        },
                    );
                }};
            }

            check!(op_sub, Prim::F64(7.0));
            check!(op_mul, Prim::F64(18.0));
            check!(op_div, Prim::F64(4.5));
            check!(op_rdiv, Prim::F64(2.0 / 9.0));
            check!(op_ediv, Prim::Int(9.0_f64.div_euclid(2.0) as i128));
            check!(op_rediv, Prim::Int(2.0_f64.div_euclid(9.0) as i128));
            check!(op_mod, Prim::F64(9.0_f64.rem_euclid(2.0)));
            check!(op_rmod, Prim::F64(2.0_f64.rem_euclid(9.0)));
        });
    }

    #[test]
    fn f64_op_hash_matches_prim_hash_and_op_type_is_float_singleton() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_float(strand, 9.0, Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    use std::hash::Hasher;

                    let mut hasher = DefaultHasher::new();
                    f64::op_hash(recv, strand, &mut hasher).unwrap();

                    let mut expected_hasher = DefaultHasher::new();
                    Prim::from(9.0_f64).op_hash(strand, &mut expected_hasher);

                    assert_eq!(hasher.finish(), expected_hasher.finish());
                });
            strand
                .builtin_types()
                .f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    f64::op_type(recv, strand, Slot::reborrow(&mut out));
                });
            let result: &Value = &out;
            assert!(result.eq(strand, &strand.singletons().float));
        });
    }

    #[test]
    fn verbatim_preserves_original_text_and_computed_value() {
        with_vm(async |strand, [mut slot]| {
            make_verbatim(strand, 1.5, "1.5e0", Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .verbatim_f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut verbatim = String::new();
                    Verbatim::op_verbatim(recv.clone(), strand, &mut verbatim).unwrap();
                    assert_eq!(verbatim, "1.5e0");

                    let mut display = String::new();
                    Verbatim::op_display(recv.clone(), strand, &mut display).unwrap();
                    assert_eq!(display, "1.5");

                    assert!(Verbatim::op_bool(recv.clone(), strand));

                    let neg = Verbatim::op_neg(recv, strand).unwrap();
                    assert_eq!(neg.to_prim(strand).unwrap(), Prim::F64(-1.5));
                });
        });
    }

    #[test]
    fn verbatim_op_debug_quotes_the_computed_value() {
        with_vm(async |strand, [mut slot]| {
            make_verbatim(strand, 1.5, "1.5e0", Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .verbatim_f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut s = String::new();
                    Verbatim::op_debug(recv, strand, &mut s).unwrap();
                    assert_eq!(s, format!("{:?}", 1.5_f64));
                });
        });
    }

    #[test]
    fn verbatim_remaining_arithmetic_ops_delegate_to_prim() {
        with_vm(async |strand, [mut slot, mut other_slot]| {
            Output::set(strand, &mut other_slot, 2.0_f64);

            macro_rules! check {
                ($op:ident, $expected:expr) => {{
                    make_verbatim(strand, 9.0, "9.0", Slot::reborrow(&mut slot));
                    let value: &Value = &slot;
                    strand
                        .builtin_types()
                        .verbatim_f64
                        .cast(value)
                        .unwrap()
                        .enter_sync(strand, |strand, recv| {
                            let result = Verbatim::$op(recv, strand, &other_slot).unwrap();
                            assert_eq!(result.to_prim(strand).unwrap(), $expected);
                        });
                }};
            }

            check!(op_sub, Prim::F64(7.0));
            check!(op_mul, Prim::F64(18.0));
            check!(op_div, Prim::F64(4.5));
            check!(op_rdiv, Prim::F64(2.0 / 9.0));
            check!(op_ediv, Prim::Int(9.0_f64.div_euclid(2.0) as i128));
            check!(op_rediv, Prim::Int(2.0_f64.div_euclid(9.0) as i128));
            check!(op_mod, Prim::F64(9.0_f64.rem_euclid(2.0)));
            check!(op_rmod, Prim::F64(2.0_f64.rem_euclid(9.0)));
        });
    }

    #[test]
    fn verbatim_op_hash_matches_prim_hash_and_op_type_is_float_singleton() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_verbatim(strand, 9.0, "9.0", Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            strand
                .builtin_types()
                .verbatim_f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    use std::hash::Hasher;

                    let mut hasher = DefaultHasher::new();
                    Verbatim::op_hash(recv, strand, &mut hasher).unwrap();

                    let mut expected_hasher = DefaultHasher::new();
                    Prim::from(9.0_f64).op_hash(strand, &mut expected_hasher);

                    assert_eq!(hasher.finish(), expected_hasher.finish());
                });
            strand
                .builtin_types()
                .verbatim_f64
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Verbatim::op_type(recv, strand, Slot::reborrow(&mut out));
                });
            let result: &Value = &out;
            assert!(result.eq(strand, &strand.singletons().float));
        });
    }

    #[test]
    fn coerce_parses_strings_and_converts_prim_variants() {
        with_vm(async |strand, [mut slot]| {
            Output::set(strand, &mut slot, "3.5");
            assert_eq!(coerce(&slot, strand).unwrap(), 3.5);

            Output::set(strand, &mut slot, "not a float");
            assert_eq!(coerce(&slot, strand).unwrap_err().kind(), ErrorKind::Type);

            Output::set(strand, &mut slot, 4_i64);
            assert_eq!(coerce(&slot, strand).unwrap(), 4.0);

            Output::set(strand, &mut slot, true);
            assert_eq!(coerce(&slot, strand).unwrap(), 1.0);

            slot.store(Value::NIL);
            assert_eq!(coerce(&slot, strand).unwrap_err().kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn construct_accepts_float_and_representable_int_rejects_others() {
        with_vm(async |strand, [mut slot]| {
            Output::set(strand, &mut slot, 1.25_f64);
            assert_eq!(construct(&slot, strand).unwrap(), 1.25);

            Output::set(strand, &mut slot, 42_i64);
            assert_eq!(construct(&slot, strand).unwrap(), 42.0);

            Output::set(strand, &mut slot, true);
            assert_eq!(
                construct(&slot, strand).unwrap_err().kind(),
                ErrorKind::Type
            );
        });
    }

    #[test]
    fn float_type_op_call_succeeds_and_rejects_unconvertible() {
        with_vm(async |strand, [mut out]| {
            call!(strand, &strand.singletons().float, &mut out, 7_i64)
                .await
                .unwrap();
            let result: &Value = &out;
            assert_eq!(result.to_prim(strand).unwrap(), Prim::F64(7.0));

            let err = call!(strand, &strand.singletons().float, &mut out, true)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn float_type_op_get_known_method_succeeds_unknown_field_errors() {
        with_vm(async |strand, [mut ok_out, unused_out]| {
            let float_recv = strand
                .builtin_types()
                .float_type
                .cast(&strand.singletons().float)
                .unwrap();
            float_recv.enter_sync(strand, |strand, recv| {
                Float::op_get(
                    recv,
                    strand,
                    Sym::well_known(sym::ADD_METHOD),
                    Slot::reborrow(&mut ok_out),
                )
                .unwrap();
            });
            let bound: &Value = &ok_out;
            assert!(!bound.is_nil());

            let float_recv = strand
                .builtin_types()
                .float_type
                .cast(&strand.singletons().float)
                .unwrap();
            float_recv.enter_sync(strand, |strand, recv| {
                let err =
                    Float::op_get(recv, strand, Sym::well_known(sym::LEN), unused_out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
            });
        });
    }

    #[test]
    fn float_type_op_mcall_default_dispatch_reaches_op_add() {
        with_vm(async |strand, [mut out]| {
            method!(
                strand,
                &strand.singletons().float,
                Sym::well_known(sym::ADD_METHOD),
                &mut out,
                1.5_f64,
                2.5_f64
            )
            .await
            .unwrap();
            let result: &Value = &out;
            assert_eq!(result.to_prim(strand).unwrap(), Prim::F64(4.0));
        });
    }
}
