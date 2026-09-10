pub mod fmt;
pub(crate) mod prim;
pub(crate) mod repr;
pub mod view;

use std::{
    cell::UnsafeCell,
    convert::Into,
    hash::DefaultHasher,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ops::{ControlFlow, Deref},
    ptr, result, str,
};

use crate::{
    Program,
    arg::Args,
    error::{Error, ErrorKind, Result},
    frame::{Frame, Upvars},
    gc::{self, Base, BaseBorrow, BaseWeak, Collect, Gc, arena},
    method,
    object::{
        self, BoundMethod, array, backtrace,
        class::{self, iter_natives},
        dict,
        function::{Function, NativeFunction, NativeFunctionAnnex},
        protocol::{
            Delegated, Dispatch, GcObj, Header, Inspect, Protocol, Spread, SpreadContext,
            TypeHandle,
        },
        set, tuple,
    },
    sig::Unpack,
    stdlib::fmt as fmt_obj,
    strand::Strand,
    sym::{self, Sym},
    vm::{Alloc, Vm},
};

use prim::Prim;
use repr::{Decode, Repr};
use view::{
    Array, Bin, Dict, Fmt, FmtParam, FmtValue, ObjectView, Range, Record, Set, Str, Tuple, View,
};

use self::fmt::{Format, Spec};

pub(crate) enum Case<'v, 'a> {
    Prim(Prim),
    Object(BaseBorrow<'v, 'a, Header>),
}

/// Do value.
///
/// Represents a value in Do.  Owned instances of this type are not acquirable
/// directly in order to guarantee that all live Do values are trackable by
/// the garbage collector.  A [`Slot`] is the usual way to receive or store an
/// owned value temporarily.  For holding a value long-term, use a [`Root`]
/// or the [`Object::SLOTS`](crate::object::native::Object::SLOTS) mechanism.
pub struct Value<'v>(Repr, PhantomData<&'v mut &'v ()>);

/// Growable binary buffer that can be finalized directly into a Do `Bin` or `Str`.
///
/// This is useful when bytes arrive incrementally and an intermediate
/// allocation would otherwise be needed before constructing a Do value.
pub struct BinEmbryo<'v> {
    embryo: Option<gc::Embryo<'v, Header, [u8]>>,
}

/// Growable UTF-8 buffer that can be finalized directly into a Do `Str`.
///
/// Safe methods on this type preserve the invariant that the initialized prefix
/// is valid UTF-8. The unsafe spare-capacity/advance path may be used when the
/// caller can uphold that invariant manually.
pub struct StrEmbryo<'v> {
    embryo: Option<gc::Embryo<'v, Header, [u8]>>,
}

unsafe impl<'v> Collect for Value<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn arena::Visit) -> ControlFlow<()> {
        match self.0.decode() {
            Decode::Prim(_) => ControlFlow::Continue(()),
            Decode::Object(raw) => visit.visit(raw.cast()),
        }
    }

    fn clear(&mut self) {
        *self = Value::NIL;
    }
}

impl<'v> Value<'v> {
    pub(crate) const NIL: Self = Self(Repr::NIL, PhantomData);
    pub(crate) const FALSE: Self = Self(Repr::FALSE, PhantomData);
    pub(crate) const TRUE: Self = Self(Repr::TRUE, PhantomData);

    pub(crate) fn repr_eq(&self, vm: &Vm<'v>, mut other: impl Input<'v>) -> bool {
        match other.input_take(vm, private::Sealed) {
            InputBy::Borrow(value) => value.0 == self.0,
            InputBy::Value(value, slot) => {
                let res = value.0 == self.0;
                if let Some(mut slot) = slot {
                    slot.store(value)
                }
                res
            }
        }
    }

    pub(crate) fn dup(&self) -> Self {
        match self.case() {
            Case::Object(base) => base.upcast::<arena::Header>().base_get().retain(),
            Case::Prim(_) => (),
        }
        Self(self.0.clone(), PhantomData)
    }

    #[expect(dead_code)]
    pub(crate) fn downgrade(&self) -> Weak<'v> {
        Weak::from_value(self)
    }

    pub(crate) fn from_prim(vm: &Vm<'v>, value: Prim) -> Self {
        match value {
            Prim::Nil => Value::NIL,
            Prim::Int(v) => Value::from_int(vm, v),
            Prim::F64(v) => Value::from_f64(vm, v),
            Prim::Bool(v) => Value::from_bool(v),
        }
    }

    pub(crate) fn from_bool(value: bool) -> Self {
        if value { Value::TRUE } else { Value::FALSE }
    }

    pub(crate) fn from_int(vm: &Vm<'v>, value: i128) -> Self {
        if let Ok(value) = i64::try_from(value)
            && let Some(repr) = Repr::from_i64(value)
        {
            Self(repr, PhantomData)
        } else {
            Value::from_object(GcObj::new(vm.arena(), vm.builtin_types().int, value))
        }
    }

    #[inline]
    pub(crate) fn from_i64(vm: &Vm<'v>, value: i64) -> Self {
        Self::from_int(vm, value.into())
    }

    pub(crate) fn from_f64(vm: &Vm<'v>, value: f64) -> Self {
        if let Some(repr) = Repr::from_f64(value) {
            Self(repr, PhantomData)
        } else {
            Value::from_object(GcObj::new(vm.arena(), vm.builtin_types().f64, value))
        }
    }

    pub(crate) fn from_int_verbatim(vm: &Vm<'v>, value: i128, text: &str) -> Self {
        Value::from_object(GcObj::new(
            vm.arena(),
            vm.builtin_types().verbatim_int,
            object::int::Verbatim::new(value, text),
        ))
    }

    pub(crate) fn from_f64_verbatim(vm: &Vm<'v>, value: f64, text: &str) -> Self {
        Value::from_object(GcObj::new(
            vm.arena(),
            vm.builtin_types().verbatim_f64,
            object::float::Verbatim::new(value, text),
        ))
    }

    pub(crate) fn from_str(vm: &Vm<'v>, value: &str) -> Self {
        Self::from_object(gc::Base::upcast(unsafe {
            gc::Base::from_header_utf8_slice(
                vm.arena(),
                Header::new(vm.arena(), vm.builtin_types().str.vtbl),
                value.as_bytes(),
            )
        }))
    }

    pub(crate) fn from_u8_slice(vm: &Vm<'v>, value: &[u8]) -> Self {
        Self::from_object(gc::Base::upcast(unsafe {
            gc::Base::from_header_slice(
                vm.arena(),
                Header::new(vm.arena(), vm.builtin_types().bin.vtbl),
                value,
            )
        }))
    }

    pub(crate) fn from_object<T: ?Sized + Protocol<'v>>(value: GcObj<'v, T>) -> Self {
        Self(Repr::from_object(value), PhantomData)
    }

    pub(crate) fn from_input(vm: &Vm<'v>, mut input: impl Input<'v>) -> Self {
        match input.input_take(vm, private::Sealed) {
            InputBy::Borrow(value) => value.dup(),
            InputBy::Value(value, _) => value,
        }
    }

    /// # Safety
    /// The uninit state must be cleared by `put` before accessing the
    /// value by any other means or causing it to be dropped.
    pub(crate) unsafe fn set_uninit(&mut self) {
        self.0 = Repr::UNINIT;
    }

    /// Tests whether this value is an instance of a type, following nominal
    /// supertypes as the `isa` operator does.
    pub fn is_instance_of(&self, strand: &mut Strand<'v, '_>, input: impl Input<'v>) -> bool {
        strand.with_slots_sync(|strand, [mut self_ty, mut ty]| {
            self.op_type(strand, Slot::reborrow(&mut self_ty));
            Output::set(strand, &mut ty, input);
            self_ty.op_subtype(strand, &ty)
        })
    }

    pub(crate) fn is_uninit(&self) -> bool {
        self.0.is_uninit()
    }

    pub(crate) fn store(&mut self, value: Self) {
        self.0 = value.0.clone();
        mem::forget(value);
    }

    pub(crate) fn case(&self) -> Case<'v, '_> {
        unsafe {
            match self.0.decode() {
                Decode::Prim(prim) => Case::Prim(prim),
                Decode::Object(ptr) => Case::Object(BaseBorrow::new(ptr)),
            }
        }
    }

    pub(crate) fn take(&mut self) -> Self {
        mem::replace(self, Value::NIL)
    }

    pub(crate) fn accept(&self, visit: &mut dyn arena::Visit) -> ControlFlow<()> {
        match self.0.decode() {
            Decode::Prim(_) => ControlFlow::Continue(()),
            Decode::Object(raw) => visit.visit(raw.cast()),
        }
    }

    pub(crate) async fn op_call<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_call(strand, args, out).await,
            Case::Prim(o) => Err(Error::type_error(
                strand,
                format!("call not supported: {o}"),
            )),
        }
    }

    pub(crate) async fn op_mcall<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_mcall(strand, method, args, out).await,
            Case::Prim(prim) => prim.op_mcall(strand, method, args, out).await,
        }
    }

    pub(crate) fn op_fill<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_fill(strand, type_obj, native),
            _ => Err(Error::type_error(strand, "fill not supported")),
        }
    }

    /// The [`ErrorKind`] a native [`object::native::Object`] type declared for
    /// its instances, via an `std` error variant among its nominal supertypes.
    ///
    /// Such a type has its own representation and so cannot carry a boxed error
    /// the way a Do subclass of an error variant does; this is how it answers
    /// the classification half of the error interface instead.
    pub(crate) fn declared_error_kind(&self, vm: &Vm<'v>) -> Option<ErrorKind> {
        let Case::Object(o) = self.case() else {
            return None;
        };
        let vtbl: *const object::protocol::Vtbl<'v> = unsafe { o.base_get().vtbl() };
        vm.error_kind_vtbls
            .iter()
            .find(|candidate| ptr::eq(candidate.as_ptr().cast_const().cast(), vtbl))
            .and_then(|candidate| unsafe { candidate.as_ref() }.error_kind)
    }

    pub(crate) fn downcast_ref<T: ?Sized + Protocol<'v>>(
        &self,
        handle: TypeHandle<'v, T>,
    ) -> Option<gc::Borrow<'v, '_, Header, T>> {
        match &self.case() {
            Case::Object(o) => unsafe {
                if ptr::eq(o.base_get().vtbl() as *const _, handle.vtbl() as *const _) {
                    Some(gc::Borrow::from_raw(o.into_raw().cast()))
                } else {
                    None
                }
            },
            Case::Prim(_) => None,
        }
    }

    /// Downcast `self` to the native type `T`, looking through [`ClassInstance`] native
    /// slots when a direct downcast fails.
    ///
    /// Tries [`Value::downcast_ref`] on `self` directly first.  If that returns [`None`]
    /// and `self` is a [`ClassInstance`], tries [`Value::downcast_ref`] against each
    /// initialized native slot in turn, returning the first match.
    pub(crate) fn downcast_native<'a, T: ?Sized + Protocol<'v>>(
        &'a self,
        vm: &Vm<'v>,
        vtbl: TypeHandle<'v, T>,
    ) -> Option<gc::Borrow<'v, 'a, Header, T>> {
        self.downcast_native_with(vm.builtin_types().class_instance, vtbl)
    }

    /// [`Value::downcast_native`] against a pre-obtained [`ClassInstance`] type handle.
    ///
    /// Handles are plain vtable pointers, so a caller that has one cached (such as
    /// [`Type`](crate::object::Type)) can look through subclass slots without holding a
    /// [`Vm`] reference — which matters in contexts where none exists, such as
    /// [`Object::finalize`](crate::object::Object::finalize).
    pub(crate) fn downcast_native_with<'a, T: ?Sized + Protocol<'v>>(
        &'a self,
        class_instance: TypeHandle<'v, class::ClassInstance<'v>>,
        vtbl: TypeHandle<'v, T>,
    ) -> Option<gc::Borrow<'v, 'a, Header, T>> {
        if let Some(borrow) = self.downcast_ref(vtbl) {
            return Some(borrow);
        }
        let ci_borrow = self.downcast_ref(class_instance)?;
        for slot_val in iter_natives(ci_borrow) {
            if let Some(borrow) = slot_val.downcast_ref(vtbl) {
                return Some(borrow);
            }
        }
        None
    }

    pub(crate) fn op_verbatim<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Prim(prim) => crate::fmt!(strand, w, "{}", prim),
            Case::Object(o) => o.op_verbatim(strand, w),
        }
    }

    pub(crate) fn op_display<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Prim(prim) => crate::fmt!(strand, w, "{}", prim),
            Case::Object(o) => o.op_display(strand, w),
        }
    }

    pub(crate) fn op_type<'a, 's>(&self, strand: &'a mut Strand<'v, 's>, mut out: Slot<'v, 'a>) {
        match self.case() {
            Case::Prim(prim) => {
                let builtin = strand.singletons();
                out.store(match prim {
                    Prim::Nil => builtin.nil.dup(),
                    Prim::Int(_) => builtin.int.dup(),
                    Prim::F64(_) => builtin.float.dup(),
                    Prim::Bool(_) => builtin.bool.dup(),
                })
            }
            Case::Object(o) => o.op_type(strand, out),
        }
    }

    pub(crate) fn op_subtype<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        match self.case() {
            Case::Prim(prim) => {
                // Primitives are only subtypes of themselves
                let self_type = match prim {
                    Prim::Nil => &strand.singletons().nil,
                    Prim::Int(_) => &strand.singletons().int,
                    Prim::F64(_) => &strand.singletons().float,
                    Prim::Bool(_) => &strand.singletons().bool,
                };
                self_type.op_eq(strand, supertype).op_bool(strand)
            }
            Case::Object(o) => o.op_subtype(strand, supertype),
        }
    }

    pub(crate) fn op_inspect(&self, vm: &Vm<'v>) -> Option<Inspect<'v, '_>> {
        match self.case() {
            Case::Prim(_) => None,
            Case::Object(o) => o.op_inspect(vm),
        }
    }

    pub(crate) fn op_debug<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Prim(prim) => crate::fmt!(strand, w, "{}", prim),
            Case::Object(o) => o.op_debug(strand, w),
        }
    }

    pub(crate) fn op_fmt<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Prim(prim) => fmt::format_prim(prim, strand, spec, w),
            Case::Object(object) => object.op_fmt(strand, spec, w),
        }
    }

    pub(crate) fn to_prim<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Prim> {
        match &self.case() {
            Case::Prim(p) => Ok(*p),
            Case::Object(_) => {
                let bt = strand.builtin_types();
                let vtbl_int = bt.int;
                let vtbl_f64 = bt.f64;
                let vtbl_vint = bt.verbatim_int;
                let vtbl_vf64 = bt.verbatim_f64;
                let vtbl_ci = bt.class_instance;
                // Direct downcasts for the four boxed primitive types.
                if let Some(b) = self.downcast_ref(vtbl_int) {
                    return Ok((*b.get()).into());
                }
                if let Some(b) = self.downcast_ref(vtbl_f64) {
                    return Ok((*b.get()).into());
                }
                if let Some(b) = self.downcast_ref(vtbl_vint) {
                    return Ok(b.get().value.into());
                }
                if let Some(b) = self.downcast_ref(vtbl_vf64) {
                    return Ok(b.get().value.into());
                }
                // For ClassInstance subclasses of numeric types, check each native slot.
                // int/int::Verbatim share a type object (Int), as do f64/float::Verbatim
                // (Float), so both verbatim and non-verbatim may appear in the same slot.
                if let Some(ci) = self.downcast_ref(vtbl_ci) {
                    for slot_val in iter_natives(ci) {
                        match slot_val.case() {
                            Case::Prim(p) => return Ok(p),
                            Case::Object(_) => {
                                if let Some(b) = slot_val.downcast_ref(vtbl_int) {
                                    return Ok((*b.get()).into());
                                }
                                if let Some(b) = slot_val.downcast_ref(vtbl_f64) {
                                    return Ok((*b.get()).into());
                                }
                                if let Some(b) = slot_val.downcast_ref(vtbl_vint) {
                                    return Ok(b.get().value.into());
                                }
                                if let Some(b) = slot_val.downcast_ref(vtbl_vf64) {
                                    return Ok(b.get().value.into());
                                }
                            }
                        }
                    }
                }
                Err(Error::type_error(strand, "not a primitive type"))
            }
        }
    }

    pub(crate) fn op_neg<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        match &self.case() {
            Case::Prim(prim) => prim.op_neg(strand).map(|p| Value::from_prim(strand, p)),
            Case::Object(object) => object.op_neg(strand),
        }
    }

    pub(crate) fn op_not<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(!self.op_bool(strand)))
    }

    pub(crate) fn op_bnot<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        match &self.case() {
            Case::Prim(prim) => {
                let value = prim.op_bnot(strand)?;
                Ok(Value::from_prim(strand, value))
            }
            Case::Object(object) => object.op_bnot(strand),
        }
    }

    fn binop<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
        prim: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
        lobj: for<'b> fn(
            &BaseBorrow<'v, 'b, Header>,
            &'b mut Strand<'v, 's>,
            &Value<'v>,
        ) -> Result<'v, 's, Value<'v>>,
        robj: for<'b> fn(
            &BaseBorrow<'v, 'a, Header>,
            &'b mut Strand<'v, 's>,
            &Value<'v>,
        ) -> Result<'v, 's, Value<'v>>,
    ) -> Result<'v, 's, Value<'v>> {
        match match (self.case(), other.case()) {
            // Both primitives: dispatch to primitive handler
            (Case::Prim(left), Case::Prim(right)) => {
                prim(&left, strand, &right).map(|v| Value::from_prim(strand, v))
            }
            // Object only on left: dispatch to left object handler
            (Case::Object(left), Case::Prim(_)) => lobj(&left, strand, other),
            // Object only on right: dispatch to right object handler
            (Case::Prim(_), Case::Object(right)) => robj(&right, strand, self),
            // Object on both sides: try left object handler, fall back to right if unsupported
            (Case::Object(left), Case::Object(right)) => match lobj(&left, strand, other) {
                Ok(v) => Ok(v),
                Err(err) if err.kind() == ErrorKind::Unsupported => robj(&right, strand, self),
                other => other,
            },
        } {
            Err(err) if err.kind() == ErrorKind::Unsupported => {
                Err(Error::type_error(strand, "no overload available"))
            }
            other => other,
        }
    }

    // Commutatative binary operation
    fn binop_comm<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
        prim: fn(&Prim, &mut Strand<'v, 's>, &Prim) -> Result<'v, 's, Prim>,
        obj: for<'b> fn(
            &BaseBorrow<'v, 'b, Header>,
            &'b mut Strand<'v, 's>,
            &Value<'v>,
        ) -> Result<'v, 's, Value<'v>>,
    ) -> Result<'v, 's, Value<'v>> {
        // Left/right object handlers are symmetric
        self.binop(strand, other, prim, obj, obj)
    }

    pub(crate) fn op_band<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_band, |this, strand, other| {
            this.op_band(strand, other)
        })
    }

    pub(crate) fn op_bor<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_bor, |this, strand, other| {
            this.op_bor(strand, other)
        })
    }

    pub(crate) fn op_bxor<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_bxor, |this, strand, other| {
            this.op_bxor(strand, other)
        })
    }

    pub(crate) fn op_shl<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_shl, |this, strand, other| {
            this.op_shl(strand, other)
        })
    }

    pub(crate) fn op_shr<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_shr, |this, strand, other| {
            this.op_shr(strand, other)
        })
    }

    pub(crate) fn op_add<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_add, |this, strand, other| {
            this.op_add(strand, other)
        })
    }

    pub(crate) fn op_sub<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_sub,
            |this, strand, other| this.op_sub(strand, other),
            |this, strand, other| this.op_rsub(strand, other),
        )
    }

    pub(crate) fn op_mul<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop_comm(strand, other, Prim::op_mul, |this, strand, other| {
            this.op_mul(strand, other)
        })
    }

    pub(crate) fn op_div<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_div,
            |this, strand, other| this.op_div(strand, other),
            |this, strand, other| this.op_rdiv(strand, other),
        )
    }

    pub(crate) fn op_ediv<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_ediv,
            |this, strand, other| this.op_ediv(strand, other),
            |this, strand, other| this.op_rediv(strand, other),
        )
    }

    pub(crate) fn op_mod<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_mod,
            |this, strand, other| this.op_mod(strand, other),
            |this, strand, other| this.op_rmod(strand, other),
        )
    }

    // Reverse operations: receiver is the right operand, argument is the left operand

    pub(crate) fn op_rsub<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_rsub,
            |this, strand, other| this.op_rsub(strand, other),
            |this, strand, other| this.op_sub(strand, other),
        )
    }

    pub(crate) fn op_rdiv<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_rdiv,
            |this, strand, other| this.op_rdiv(strand, other),
            |this, strand, other| this.op_div(strand, other),
        )
    }

    pub(crate) fn op_rediv<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_rediv,
            |this, strand, other| this.op_rediv(strand, other),
            |this, strand, other| this.op_ediv(strand, other),
        )
    }

    pub(crate) fn op_rmod<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_rmod,
            |this, strand, other| this.op_rmod(strand, other),
            |this, strand, other| this.op_mod(strand, other),
        )
    }

    pub(crate) fn op_eq<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Value<'v> {
        if self.repr_eq(strand, other) {
            return Value::TRUE;
        }
        self.binop_comm(
            strand,
            other,
            |l, strand, r| Ok(l.op_eq(strand, r).into()),
            |this, strand, other| this.op_eq(strand, other),
        )
        .unwrap_or(Value::FALSE)
    }

    pub(crate) fn op_ne<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Value<'v> {
        if self.repr_eq(strand, other) {
            return Value::FALSE;
        }
        self.binop_comm(
            strand,
            other,
            |l, strand, r| Ok(l.op_ne(strand, r).into()),
            |this, strand, other| this.op_ne(strand, other),
        )
        .unwrap_or(Value::TRUE)
    }

    pub(crate) fn op_lt<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_lt,
            |this, strand, other| this.op_lt(strand, other),
            |this, strand, other| this.op_gt(strand, other),
        )
    }

    pub(crate) fn op_gt<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_gt,
            |this, strand, other| this.op_gt(strand, other),
            |this, strand, other| this.op_lt(strand, other),
        )
    }

    pub(crate) fn op_lte<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_lte,
            |this, strand, other| this.op_lte(strand, other),
            |this, strand, other| this.op_gte(strand, other),
        )
    }

    pub(crate) fn op_gte<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &'a Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        self.binop(
            strand,
            other,
            Prim::op_gte,
            |this, strand, other| this.op_gte(strand, other),
            |this, strand, other| this.op_lte(strand, other),
        )
    }

    pub(crate) fn function(
        vm: &Vm<'v>,
        loaded: Gc<'v, Program<'v>>,
        upvars: Option<Gc<'v, Upvars<'v>>>,
        id: usize,
    ) -> Self {
        Self::from_object(GcObj::new(
            vm.arena(),
            vm.builtin_types().function,
            Function::new(loaded, upvars, id),
        ))
    }

    pub(crate) fn op_bool<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> bool {
        match &self.case() {
            Case::Prim(p) => p.op_bool(strand),
            Case::Object(o) => o.op_bool(strand),
        }
    }

    pub(crate) fn op_get<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_get(strand, field, out),
            Case::Prim(prim) => prim.op_get(self, strand, field, out),
        }
    }

    pub(crate) fn op_set<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_set(strand, field, value),
            _ => Err(Error::type_error(strand, "field set not supported")),
        }
    }

    pub(crate) fn op_index<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_index(strand, index, out),
            _ => Err(Error::type_error(strand, "indexing not supported")),
        }
    }

    pub(crate) fn op_assign<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_assign(strand, index, value),
            _ => Err(Error::type_error(strand, "index assignment not supported")),
        }
    }

    pub(crate) fn op_hash<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Prim(prim) => {
                prim.op_hash(strand, hasher);
                Ok(())
            }
            Case::Object(o) => o.op_hash(strand, hasher),
        }
    }

    pub(crate) async fn op_next<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        match self.case() {
            Case::Object(o) => o.op_next(strand, out).await,
            _ => Err(Error::type_error(strand, "iterator `next` not supported")),
        }
    }

    pub(crate) async fn op_put<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_put(strand, item).await,
            Case::Prim(_) => Err(Error::type_error(strand, "sink `put` not supported")),
        }
    }

    pub(crate) async fn op_iter<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_iter(strand, out).await,
            Case::Prim(_) => Err(Error::type_error(strand, "iteration not supported")),
        }
    }

    pub(crate) async fn op_sink<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_sink(strand, out).await,
            Case::Prim(_) => Err(Error::type_error(strand, "sink protocol not supported")),
        }
    }

    pub(crate) async fn op_spread<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_spread(strand, context, sink).await,
            Case::Prim(_) => Err(Error::type_error(strand, "spread not supported")),
        }
    }

    pub(crate) async fn op_unpack<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.case() {
            Case::Object(o) => o.op_unpack(strand, sig, out).await,
            Case::Prim(_) => Err(Error::type_error(
                strand,
                "argument expansion not supported",
            )),
        }
    }

    pub(crate) fn to_index<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.to_prim(strand)?.to_index(strand)
    }

    /// Writes the verbatim representation of this value to `format`.
    #[inline]
    pub fn verbatim<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        format: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_verbatim(strand, format)
    }

    /// Writes the human-readable representation of this value to `format`.
    #[inline]
    pub fn display<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        format: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_display(strand, format)
    }

    /// Writes the debug representation of this value to `format`.
    #[inline]
    pub fn debug<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        format: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_debug(strand, format)
    }

    /// Writes this value according to a resolved formatting specification.
    #[inline]
    pub fn fmt<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        format: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_fmt(strand, spec, format)
    }

    /// Converts a value to its verbatim representation.
    /// See the documentation for `std.verbatim` in the language documentation for details.
    #[inline]
    pub fn to_verbatim<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, String> {
        let mut out = String::new();
        self.verbatim(strand, &mut out)?;
        Ok(out)
    }

    /// Returns a human-readable representation of the value
    #[inline]
    pub fn to_string<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, String> {
        let mut out = String::new();
        self.display(strand, &mut out)?;
        Ok(out)
    }

    /// Returns a string representation of the value for debugging purposes
    #[inline]
    pub fn to_debug<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, String> {
        let mut out = String::new();
        self.debug(strand, &mut out)?;
        Ok(out)
    }

    /// Returns the "truth" value of the value.  In particular:
    /// - `nil` is false
    /// - `false` is false and `true` is true
    /// - `0` is false and all other integers are true
    /// - Empty sequences/containers are false, others are true
    /// - Empty strings are false, others are true
    /// - Other objects are usually `true` unless they have special behavior
    #[inline]
    pub fn to_bool<'a, 's>(&self, strand: &'a mut Strand<'v, 's>) -> bool {
        self.op_bool(strand)
    }

    /// Tests whether two values are equal.
    #[inline]
    pub fn eq<'a, 's>(&self, strand: &'a mut Strand<'v, 's>, other: impl Input<'v>) -> bool {
        self.op_eq(strand, &Value::from_input(strand, other))
            .op_bool(strand)
    }

    /// Tests whether two values are unequal.
    #[inline]
    pub fn ne<'a, 's>(&self, strand: &'a mut Strand<'v, 's>, other: impl Input<'v>) -> bool {
        self.op_ne(strand, &Value::from_input(strand, other))
            .op_bool(strand)
    }

    /// Tests if the value is `nil`
    #[inline]
    pub fn is_nil(&self) -> bool {
        matches!(self.case(), Case::Prim(Prim::Nil))
    }

    /// Create a bound method object with `self` as the receiver.
    /// Note that this succeeds even if `method` isn't a valid method for the receiver;
    /// an error will be raised on invocation.
    pub fn bind_method(
        &self,
        alloc: &mut impl Alloc<'v>,
        method: Sym<'v, '_>,
        mut out: impl Output<'v>,
    ) {
        BoundMethod::create(alloc, self, method, Slot::from_output(&mut out));
    }

    /// Invoke callable.
    ///
    /// Unless you are forwarding an existing [`Args`], use the [`call!()`](crate::call)
    /// macro to invoke a callable value with a suitable argument pack
    pub async fn call<'a, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_call(strand, args, Slot::from_output(&mut out))
            .await
    }

    /// Invoke method.
    ///
    /// Unless you are forwarding an existing [`Args`], use the [`method!()`](crate::method)
    /// macro to invoke a method with a suitable argument pack
    pub async fn method<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_mcall(strand, method, args, Slot::from_output(&mut out))
            .await
    }

    /// Get a field.
    pub fn get<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_get(strand, field, Output::as_slot(&mut out, private::Sealed))
    }

    /// Set a field.
    pub fn set<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        let mut temp = Value::NIL;
        let mut slot = Slot::new(&mut temp);
        Output::set(strand, &mut slot, value);
        self.op_set(strand, field, slot)
    }

    /// Get an element at the given index.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The value doesn't support indexing
    /// - The index is out of bounds
    /// - The index operation fails
    ///
    /// # Example
    ///
    /// ```ignore
    /// value.index(strand, 0, &mut out)?;  // Get zero'th element
    /// value.index(strand, "key", &mut out)?;  // Get by key
    /// ```
    pub fn index<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: impl Input<'v>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let mut temp = Value::NIL;
        let mut slot = Slot::new(&mut temp);
        Output::set(strand, &mut slot, index);
        self.op_index(strand, &slot, Output::as_slot(&mut out, private::Sealed))
    }

    /// Assign a value at the given index.
    ///
    /// # Example
    ///
    /// ```ignore
    /// value.assign(strand, 0, "new value")?;  // Set zero'th element
    /// ```
    pub fn assign<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: impl Input<'v>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        let mut tempi = Value::NIL;
        let mut tempv = Value::NIL;
        let mut sloti = Slot::new(&mut tempi);
        let mut slotv = Slot::new(&mut tempv);
        Output::set(strand, &mut sloti, index);
        Output::set(strand, &mut slotv, value);
        self.op_assign(strand, sloti, slotv)
    }

    /// Obtain iterator for value
    #[inline]
    pub async fn iter<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_iter(strand, Output::as_slot(&mut out, private::Sealed))
            .await
    }

    /// Obtain sink for value
    #[inline]
    pub async fn sink<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.op_sink(strand, Output::as_slot(&mut out, private::Sealed))
            .await
    }

    /// Iterate pairs.  This is a shortcut to call the `pairs` method on the value.
    #[inline]
    pub async fn pairs<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        method!(strand, self, Sym::well_known(sym::PAIRS), out).await
    }

    /// Receive from iterator
    #[inline]
    pub async fn next<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.op_next(strand, Output::as_slot(&mut out, private::Sealed))
            .await
    }

    /// Send item to sink
    #[inline]
    pub async fn put<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        item: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        let mut temp = Value::NIL;
        let mut slot = Slot::new(&mut temp);
        Output::set(strand, &mut slot, item);
        self.op_put(strand, slot).await
    }

    /// Downcast to `Int` ([`i128`]).
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer.
    #[inline]
    pub fn to_int<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, i128> {
        match self.to_prim(strand)? {
            Prim::Int(v) => Ok(v),
            _ => Err(Error::type_error(strand, "expected Int")),
        }
    }

    /// Convert an integer value to [`i32`].
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer, or a value error if
    /// the integer is out of range for [`i32`].
    #[inline]
    pub fn to_i32<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, i32> {
        let value = self.to_int(strand)?;
        i32::try_from(value).map_err(|_| Error::value(strand, "int out of range for i32"))
    }

    /// Convert an integer value to [`u64`].
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer, or a value error if
    /// the integer is out of range for [`u64`].
    #[inline]
    pub fn to_u32<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u32> {
        let value = self.to_int(strand)?;
        u32::try_from(value).map_err(|_| Error::value(strand, "int out of range for u32"))
    }

    /// Convert an integer value to [`i64`].
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer, or a value error if
    /// the integer is out of range for [`i64`].
    #[inline]
    pub fn to_i64<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, i64> {
        let value = self.to_int(strand)?;
        i64::try_from(value).map_err(|_| Error::value(strand, "int out of range for i64"))
    }

    /// Convert an integer value to [`u64`].
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer, or a value error if
    /// the integer is out of range for [`u64`].
    #[inline]
    pub fn to_u64<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u64> {
        let value = self.to_int(strand)?;
        u64::try_from(value).map_err(|_| Error::value(strand, "int out of range for u64"))
    }

    /// Convert an integer value to [`usize`].
    ///
    /// # Errors
    /// Returns a type error if the value is not an integer, or a value error if
    /// the integer is out of range for [`usize`].
    #[inline]
    pub fn to_usize<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        let value = self.to_int(strand)?;
        usize::try_from(value).map_err(|_| Error::value(strand, "int out of range for usize"))
    }

    /// Downcast to `Int` ([`i128`]).
    ///
    /// # Returns
    /// - [`None`]: Not an `Int` value
    /// - [`Some(value)`](Some): The value as an [`i128`]
    #[inline]
    pub fn as_int(&self, strand: &mut Strand<'v, '_>) -> Option<i128> {
        match self.to_prim(strand) {
            Ok(Prim::Int(v)) => Some(v),
            _ => None,
        }
    }

    /// Test whether the value is an `Int`.
    #[inline]
    pub fn is_int(&self, strand: &mut Strand<'v, '_>) -> bool {
        self.as_int(strand).is_some()
    }

    /// Downcast to [`f64`]
    ///
    /// # Returns
    /// - [`None`]: Not an `f64` value
    /// - [`Some(value)`](Some): The value as an `f64`
    #[inline]
    pub fn as_f64(&self, strand: &mut Strand<'v, '_>) -> Option<f64> {
        match self.to_prim(strand) {
            Ok(Prim::F64(v)) => Some(v),
            _ => None,
        }
    }

    /// Downcast to [`Bool`]
    ///
    /// If you want to convert to `Bool` based on "truthiness", use [`Value::to_bool`].
    ///
    /// # Returns
    /// - [`None`]: Not a `Bool` value
    /// - [`Some(value)`](Some): The value as a `Bool`
    #[inline]
    pub fn as_bool(&self, strand: &mut Strand<'v, '_>) -> Option<bool> {
        match self.to_prim(strand) {
            Ok(Prim::Bool(v)) => Some(v),
            _ => None,
        }
    }

    /// Downcast to `Str` ([`&str`])
    ///
    /// If you want to convert to a string, use [`Value::to_string`].
    ///
    /// # Returns
    /// - [`None`]: Not a `Str` value
    /// - [`Some(value)`](Some): The value as a `Str`
    #[inline]
    pub(crate) fn as_str_raw(&self, vm: &Vm<'v>) -> Option<&str> {
        self.downcast_native(vm, vm.builtin_types().str)
            .map(|s| s.get())
    }

    /// Downcast to a `Str` witness.
    #[inline]
    pub fn as_str<'a>(&'a self, vm: &Vm<'v>) -> Option<Str<'v, 'a>> {
        Some(Str::from_value(self.as_str_raw(vm)?))
    }

    /// Downcast to `Bin` ([`&[u8]`])
    ///
    /// # Returns
    /// - [`None`]: Not a `Bin` value
    /// - [`Some(value)`](Some): The value as a `Bin`
    #[inline]
    pub(crate) fn as_bin_raw(&self, vm: &Vm<'v>) -> Option<&[u8]> {
        self.downcast_native(vm, vm.builtin_types().bin)
            .map(|s| s.get())
    }

    /// Downcast to a `Bin` witness.
    #[inline]
    pub fn as_bin<'a>(&'a self, vm: &Vm<'v>) -> Option<Bin<'v, 'a>> {
        Some(Bin::from_value(self.as_bin_raw(vm)?))
    }

    /// Downcast to [`&[u8]`].  This works for both `Str` and `Bin` types.
    ///
    /// # Returns
    /// - [`None`]: Not a `&[u8]` value
    /// - [`Some(value)`](Some): The value as a `&[u8]`
    #[inline]
    pub(crate) fn as_u8_slice_raw(&self, vm: &Vm<'v>) -> Option<&[u8]> {
        // FIXME: there should probably be a "buffer" protocol to make this generic
        self.as_bin_raw(vm)
            .or_else(|| self.as_str_raw(vm).map(str::as_bytes))
    }

    /// Downcast to symbol
    ///
    /// # Returns
    /// - [`None`]: Not a [`Sym`] value
    /// - [`Some(value)`](Some): The value as a [`Sym`]
    #[inline]
    pub fn as_sym<'a>(&self, vm: &Vm<'v>) -> Option<Sym<'v, 'a>> {
        self.downcast_ref(vm.builtin_types().sym)
            .map(|s| unsafe { Sym::from_tag(s.get().tag) })
    }

    /// Downcast value to array
    #[inline]
    pub fn as_array(&self, vm: &Vm<'v>) -> Option<Array<'v, '_>> {
        Some(Array(self.downcast_ref(vm.builtin_types().array)?))
    }

    /// Downcast value to tuple
    #[inline]
    pub fn as_tuple(&self, vm: &Vm<'v>) -> Option<Tuple<'v, '_>> {
        Some(Tuple(self.downcast_ref(vm.builtin_types().tuple)?))
    }

    /// Downcast value to dict
    #[inline]
    pub fn as_dict(&self, vm: &Vm<'v>) -> Option<Dict<'v, '_>> {
        Some(Dict(self.downcast_ref(vm.builtin_types().dict)?))
    }

    /// Downcast value to set
    #[inline]
    pub fn as_set(&self, vm: &Vm<'v>) -> Option<Set<'v, '_>> {
        Some(Set(self.downcast_ref(vm.builtin_types().set)?))
    }

    /// Downcast value to a formatted sequence
    #[inline]
    pub fn as_fmt(&self, vm: &Vm<'v>) -> Option<Fmt<'v, '_>> {
        let global = vm.state::<fmt_obj::Global<'v>>();
        Some(Fmt::from_cast(global.types.fmt.cast(self)?))
    }

    /// Downcast value to a bound interpolation
    #[inline]
    pub fn as_fmt_value(&self, vm: &Vm<'v>) -> Option<FmtValue<'v, '_>> {
        let global = vm.state::<fmt_obj::Global<'v>>();
        Some(FmtValue::from_cast(global.types.value.cast(self)?))
    }

    /// Downcast value to an unbound parameter
    #[inline]
    pub fn as_fmt_param(&self, vm: &Vm<'v>) -> Option<FmtParam<'v, '_>> {
        let global = vm.state::<fmt_obj::Global<'v>>();
        Some(FmtParam::from_cast(global.types.param.cast(self)?))
    }

    /// Downcast value to a formatting specification.
    ///
    /// A specification is plain data, so it needs no view of its own.
    #[inline]
    pub fn as_fmt_spec(&self, vm: &Vm<'v>) -> Option<Spec> {
        let global = vm.state::<fmt_obj::Global<'v>>();
        Some(
            global
                .types
                .spec
                .cast(self)?
                .enter_finalize(fmt_obj::view_spec),
        )
    }

    /// Downcast value to a read-only `range` view.
    #[inline]
    pub fn as_range(&self, vm: &Vm<'v>) -> Option<Range<'v, '_>> {
        Some(Range(self.downcast_ref(vm.builtin_types().range)?))
    }

    /// Downcast value to `strand.Backtrace`.
    #[inline]
    pub fn as_backtrace<'a>(
        &'a self,
        vm: &'a Vm<'v>,
    ) -> Option<impl ExactSizeIterator<Item = impl Frame> + 'a> {
        backtrace::iter_from_value(vm, self)
    }

    /// Inspect typed view of value
    pub fn view<'a>(&'a self, vm: &Vm<'v>) -> View<'v, 'a> {
        match self.case() {
            Case::Prim(Prim::Nil) => View::Nil,
            Case::Prim(Prim::Bool(b)) => View::Bool(b),
            Case::Prim(Prim::Int(i)) => View::Int(i),
            Case::Prim(Prim::F64(f)) => View::Float(f),
            Case::Object(obj) => {
                let bt = vm.builtin_types();
                // Boxed int
                if let Some(v) = self.downcast_native(vm, bt.int) {
                    return View::Int(*v.get());
                }
                // Verbatim int
                if let Some(v) = self.downcast_native(vm, bt.verbatim_int) {
                    return View::Int(v.get().value);
                }
                // Boxed f64
                if let Some(v) = self.downcast_native(vm, bt.f64) {
                    return View::Float(*v.get());
                }
                // Verbatim f64
                if let Some(v) = self.downcast_native(vm, bt.verbatim_f64) {
                    return View::Float(v.get().value);
                }
                // String
                if let Some(s) = self.as_str(vm) {
                    return View::Str(s);
                }
                // Binary
                if let Some(b) = self.as_bin(vm) {
                    return View::Bin(b);
                }
                // Symbol (no native subclassing)
                if let Some(sym) = self.as_sym(vm) {
                    return View::Sym(sym);
                }
                // Array
                if let Some(arr) = self.downcast_native(vm, bt.array) {
                    return View::Array(Array::from_borrow(arr));
                }
                // Dict
                if let Some(dict) = self.downcast_native(vm, bt.dict) {
                    return View::Dict(Dict::from_borrow(dict));
                }
                // Set
                if let Some(set) = self.downcast_native(vm, bt.set) {
                    return View::Set(Set::from_borrow(set));
                }
                // Record
                if let Some(record) = self.downcast_native(vm, bt.record) {
                    return View::Record(Record::from_borrow(record));
                }
                // Tuple
                if let Some(tuple) = self.downcast_native(vm, bt.tuple) {
                    return View::Tuple(Tuple::from_borrow(tuple));
                }
                // Format types. These live in stdlib state rather than the
                // builtin table, so they come last: one lookup covers the
                // four, and only a value that is none of the above pays it.
                let fmt = vm.state::<fmt_obj::Global<'v>>();
                if let Some(seq) = fmt.types.fmt.cast(self) {
                    return View::Fmt(Fmt::from_cast(seq));
                }
                if let Some(value) = fmt.types.value.cast(self) {
                    return View::FmtValue(FmtValue::from_cast(value));
                }
                if let Some(param) = fmt.types.param.cast(self) {
                    return View::FmtParam(FmtParam::from_cast(param));
                }
                if let Some(spec) = fmt.types.spec.cast(self) {
                    return View::FmtSpec(spec.enter_finalize(fmt_obj::view_spec));
                }
                // Fallback: unknown GC object
                View::Object(unsafe { ObjectView::from_ptr(obj.into_raw()) })
            }
        }
    }
}

macro_rules! delegated_binop {
    ($self:expr, $strand:expr, $other:expr, $op:ident, $reverse:ident) => {{
        let result = match ($self.receiver.case(), $other.case()) {
            (Case::Prim(_), _) => $self.receiver.$op($strand, $other),
            (Case::Object(receiver), Case::Prim(_)) => {
                Delegated::new(receiver, $self.delegator).$op($strand, $other)
            }
            (Case::Object(receiver), Case::Object(reverse)) => {
                match Delegated::new(receiver, $self.delegator).$op($strand, $other) {
                    Err(error) if error.kind() == ErrorKind::Unsupported => {
                        reverse.$reverse($strand, $self.receiver)
                    }
                    result => result,
                }
            }
        };
        match result {
            Err(error) if error.kind() == ErrorKind::Unsupported => {
                Err(Error::type_error($strand, "no overload available"))
            }
            result => result,
        }
    }};
}

impl<'v, 'a> Dispatch<'v, 'a> for Delegated<'v, 'a, &'a Value<'v>> {
    async fn op_call<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_call(strand, args, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_call(strand, args, out).await,
        }
    }

    async fn op_mcall<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_mcall(strand, method, args, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_mcall(strand, method, args, out).await,
        }
    }

    fn op_fill<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_fill(strand, type_obj, native)
            }
            Case::Prim(_) => self.receiver.op_fill(strand, type_obj, native),
        }
    }

    fn op_type<'s>(&self, strand: &'a mut Strand<'v, 's>, out: Slot<'v, 'a>) {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_type(strand, out),
            Case::Prim(_) => self.receiver.op_type(strand, out),
        }
    }

    fn op_subtype<'s>(&self, strand: &'a mut Strand<'v, 's>, supertype: &Value<'v>) -> bool {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_subtype(strand, supertype)
            }
            Case::Prim(_) => self.receiver.op_subtype(strand, supertype),
        }
    }

    fn op_inspect(&self, vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_inspect(vm),
            Case::Prim(_) => None,
        }
    }

    fn op_verbatim<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_verbatim(strand, w)
            }
            Case::Prim(_) => self.receiver.op_verbatim(strand, w),
        }
    }

    fn op_display<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_display(strand, w)
            }
            Case::Prim(_) => self.receiver.op_display(strand, w),
        }
    }

    fn op_debug<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_debug(strand, w),
            Case::Prim(_) => self.receiver.op_debug(strand, w),
        }
    }

    fn op_fmt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_fmt(strand, spec, w)
            }
            Case::Prim(_) => self.receiver.op_fmt(strand, spec, w),
        }
    }

    fn op_bool<'s>(&self, strand: &'a mut Strand<'v, 's>) -> bool {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_bool(strand),
            Case::Prim(_) => self.receiver.op_bool(strand),
        }
    }

    fn op_bnot<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_bnot(strand),
            Case::Prim(_) => self.receiver.op_bnot(strand),
        }
    }

    fn op_neg<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_neg(strand),
            Case::Prim(_) => self.receiver.op_neg(strand),
        }
    }

    fn op_band<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_band, op_band)
    }
    fn op_bor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_bor, op_bor)
    }
    fn op_bxor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_bxor, op_bxor)
    }
    fn op_shl<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_shl, op_shl)
    }
    fn op_shr<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_shr, op_shr)
    }
    fn op_add<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_add, op_add)
    }
    fn op_sub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_sub, op_rsub)
    }
    fn op_rsub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_rsub, op_sub)
    }
    fn op_mul<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_mul, op_mul)
    }
    fn op_div<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_div, op_rdiv)
    }
    fn op_rdiv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_rdiv, op_div)
    }
    fn op_ediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_ediv, op_rediv)
    }
    fn op_rediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_rediv, op_ediv)
    }
    fn op_mod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_mod, op_rmod)
    }
    fn op_rmod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_rmod, op_mod)
    }
    fn op_eq<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_eq(strand, other),
            Case::Prim(_) => Ok(self.receiver.op_eq(strand, other)),
        }
    }
    fn op_ne<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        match self.receiver.case() {
            Case::Object(receiver) => Delegated::new(receiver, self.delegator).op_ne(strand, other),
            Case::Prim(_) => Ok(self.receiver.op_ne(strand, other)),
        }
    }
    fn op_lt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_lt, op_gt)
    }
    fn op_lte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_lte, op_gte)
    }
    fn op_gt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_gt, op_lt)
    }
    fn op_gte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        delegated_binop!(self, strand, other, op_gte, op_lte)
    }

    fn op_get<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_get(strand, field, out)
            }
            Case::Prim(_) => self.receiver.op_get(strand, field, out),
        }
    }

    fn op_set<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_set(strand, field, value)
            }
            Case::Prim(_) => self.receiver.op_set(strand, field, value),
        }
    }

    fn op_index<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_index(strand, index, out)
            }
            Case::Prim(_) => self.receiver.op_index(strand, index, out),
        }
    }

    fn op_assign<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_assign(strand, index, value)
            }
            Case::Prim(_) => self.receiver.op_assign(strand, index, value),
        }
    }

    fn op_hash<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator).op_hash(strand, hasher)
            }
            Case::Prim(_) => self.receiver.op_hash(strand, hasher),
        }
    }

    async fn op_next<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_next(strand, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_next(strand, out).await,
        }
    }

    async fn op_put<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_put(strand, item)
                    .await
            }
            Case::Prim(_) => self.receiver.op_put(strand, item).await,
        }
    }

    async fn op_iter<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_iter(strand, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_iter(strand, out).await,
        }
    }

    async fn op_sink<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_sink(strand, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_sink(strand, out).await,
        }
    }

    async fn op_spread<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_spread(strand, context, sink)
                    .await
            }
            Case::Prim(_) => self.receiver.op_spread(strand, context, sink).await,
        }
    }

    async fn op_unpack<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self.receiver.case() {
            Case::Object(receiver) => {
                Delegated::new(receiver, self.delegator)
                    .op_unpack(strand, sig, out)
                    .await
            }
            Case::Prim(_) => self.receiver.op_unpack(strand, sig, out).await,
        }
    }
}

impl<'v> BinEmbryo<'v> {
    /// Creates an empty embryo without allocating.
    pub fn new() -> Self {
        Self { embryo: None }
    }

    /// Creates an empty embryo with space reserved for at least `capacity` bytes.
    pub fn new_with_capacity(alloc: &mut impl Alloc<'v>, capacity: usize) -> Self {
        let mut this = Self::new();
        this.reserve(alloc, capacity);
        this
    }

    /// Returns the number of initialized bytes currently in the embryo.
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// Returns the total byte capacity currently available without reallocating.
    pub fn capacity(&self) -> usize {
        self.embryo.as_ref().map_or(0, gc::Embryo::capacity)
    }

    /// Returns `true` if the embryo contains no initialized bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the uninitialized spare capacity of the embryo.
    ///
    /// If the embryo has not allocated yet, this returns an empty slice.
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.embryo
            .as_mut()
            .map_or(&mut [], gc::Embryo::spare_capacity_mut)
    }

    /// Ensures the embryo can accept at least `additional` more bytes
    /// without reallocating.
    pub fn reserve(&mut self, alloc: &mut impl Alloc<'v>, additional: usize) {
        if additional == 0 {
            return;
        }
        if let Some(embryo) = self.embryo.as_mut() {
            embryo.reserve(additional);
        } else {
            self.embryo = Some(Self::allocate(alloc, additional));
        }
    }

    /// Shrinks the initialized length to `len`.
    ///
    /// If `len` is greater than the current length, this is a no-op.
    pub fn truncate(&mut self, len: usize) {
        if let Some(embryo) = self.embryo.as_mut() {
            embryo.truncate(len);
        }
    }

    /// # Safety
    /// The caller must ensure the next `initialized` bytes in spare capacity were written.
    pub unsafe fn advance(&mut self, initialized: usize) {
        if initialized == 0 {
            return;
        }
        unsafe { self.embryo.as_mut().unwrap_unchecked().advance(initialized) }
    }

    /// Returns the initialized bytes currently stored in the embryo.
    pub fn as_slice(&self) -> &[u8] {
        self.embryo.as_ref().map_or(&[], gc::Embryo::as_slice)
    }

    /// Appends `slice` to the embryo.
    pub fn extend(&mut self, alloc: &mut impl Alloc<'v>, slice: &[u8]) {
        if slice.is_empty() {
            return;
        }
        self.reserve(alloc, slice.len());
        unsafe { self.embryo.as_mut().unwrap_unchecked().extend(slice) }
    }

    /// Replaces `self[start..end]` with `replacement`, shifting the tail in place.
    ///
    /// This does not build a separate staging buffer: existing bytes are shifted within the
    /// embryo's own allocation, growing first only if `replacement` is longer than the range
    /// it replaces.
    ///
    /// # Panics
    /// Panics if `start > end` or `end > self.len()`.
    pub(crate) fn splice(
        &mut self,
        alloc: &mut impl Alloc<'v>,
        start: usize,
        end: usize,
        replacement: &[u8],
    ) {
        assert!(start <= end && end <= self.len());
        let growth = replacement.len().saturating_sub(end - start);
        self.reserve(alloc, growth);
        unsafe {
            self.embryo
                .as_mut()
                .unwrap_unchecked()
                .splice(start, end, replacement)
        }
    }

    /// Like [`Self::splice`], but does not reserve capacity — the caller must have already
    /// ensured enough spare capacity exists. Useful when the replacement bytes are borrowed
    /// from a source that must not be interleaved with an allocating call (e.g. bytes
    /// obtained under [`Strand::access`](crate::strand::Strand::access), which cannot
    /// itself perform an allocating `Strand` call while the access token is live).
    ///
    /// # Panics
    /// Panics if `start > end` or `end > self.len()`.
    ///
    /// # Safety
    /// Spare capacity must be at least `replacement.len().saturating_sub(end - start)`.
    pub(crate) unsafe fn splice_unchecked(&mut self, start: usize, end: usize, replacement: &[u8]) {
        if start == end && replacement.is_empty() {
            return;
        }
        assert!(start <= end && end <= self.len());
        unsafe {
            self.embryo
                .as_mut()
                .unwrap_unchecked()
                .splice(start, end, replacement)
        }
    }

    /// Finalizes the embryo into a Do `Bin`, writes it to `out`, and resets `self` to an
    /// empty, unallocated embryo, leaving it usable afterward.
    pub(crate) fn freeze_reset(&mut self, alloc: &mut impl Alloc<'v>, out: impl Output<'v>) {
        mem::take(self).finish(alloc, out);
    }

    /// Finalizes the embryo into a Do `Bin` and writes it to `out`.
    pub fn finish(self, alloc: &mut impl Alloc<'v>, mut out: impl Output<'v>) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let value = unsafe {
            let header = Header::new(vm.arena(), vm.builtin_types().bin.vtbl);
            match self.embryo {
                Some(embryo) => Value::from_object(gc::Base::upcast(embryo.finalize(header))),
                None => Value::from_object(gc::Base::upcast(gc::Base::from_header_slice(
                    vm.arena(),
                    header,
                    &[],
                ))),
            }
        };
        Slot::from_output(&mut out).store(value);
    }

    /// Finalizes the embryo into a Do `Str` after validating it as UTF-8.
    pub fn finish_str(
        self,
        alloc: &mut impl Alloc<'v>,
        mut out: impl Output<'v>,
    ) -> result::Result<(), str::Utf8Error> {
        str::from_utf8(self.as_slice())?;
        unsafe { self.finish_str_unchecked(alloc, &mut out) };
        Ok(())
    }

    /// Finalizes the embryo into a Do `Str` *without validation*
    /// # Safety
    /// The initialized bytes must be valid UTF-8.
    pub unsafe fn finish_str_unchecked(self, alloc: &mut impl Alloc<'v>, mut out: impl Output<'v>) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let header = unsafe { Header::new(vm.arena(), vm.builtin_types().str.vtbl) };
        let value = match self.embryo {
            Some(embryo) => Value::from_object(unsafe { embryo.finalize_str_unchecked(header) }),
            None => Value::from_object(unsafe {
                gc::Base::from_header_utf8_slice(vm.arena(), header, b"")
            }),
        };
        Slot::from_output(&mut out).store(value);
    }

    fn allocate(alloc: &mut impl Alloc<'v>, capacity: usize) -> gc::Embryo<'v, Header, [u8]> {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        unsafe { gc::Embryo::<Header, [u8]>::from_arena_capacity(vm.arena(), capacity) }
    }
}

impl<'v> StrEmbryo<'v> {
    /// Creates an empty embryo without allocating.
    pub fn new() -> Self {
        Self { embryo: None }
    }

    /// Creates an empty embryo with space reserved for at least `capacity` bytes.
    pub fn new_with_capacity(alloc: &mut impl Alloc<'v>, capacity: usize) -> Self {
        let mut this = Self::new();
        this.reserve(alloc, capacity);
        this
    }

    /// Returns the number of initialized bytes currently in the embryo.
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Returns `true` if the embryo contains no initialized bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the total byte capacity currently available without reallocating.
    pub fn capacity(&self) -> usize {
        self.embryo.as_ref().map_or(0, gc::Embryo::capacity)
    }

    /// Shrinks the initialized length to `len`.
    ///
    /// If `len` is greater than the current length, this is a no-op.
    ///
    /// # Safety
    /// `len` must fall on a UTF-8 character boundary.
    pub(crate) unsafe fn truncate(&mut self, len: usize) {
        if let Some(embryo) = self.embryo.as_mut() {
            embryo.truncate(len);
        }
    }

    /// # Safety
    /// Bytes written into the returned spare capacity and later exposed via
    /// `advance` must keep the full initialized prefix valid UTF-8.
    pub unsafe fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.embryo
            .as_mut()
            .map_or(&mut [], gc::Embryo::spare_capacity_mut)
    }

    /// Ensures the embryo can accept at least `additional` more bytes
    /// without reallocating.
    pub fn reserve(&mut self, alloc: &mut impl Alloc<'v>, additional: usize) {
        if additional == 0 {
            return;
        }
        if let Some(embryo) = self.embryo.as_mut() {
            embryo.reserve(additional);
        } else {
            self.embryo = Some(BinEmbryo::allocate(alloc, additional));
        }
    }

    /// # Safety
    /// The caller must ensure the next `initialized` bytes in spare capacity were written
    /// and that advancing over them keeps the full initialized prefix valid UTF-8.
    pub unsafe fn advance(&mut self, initialized: usize) {
        if initialized == 0 {
            return;
        }
        unsafe { self.embryo.as_mut().unwrap_unchecked().advance(initialized) }
    }

    /// Returns the initialized contents as `&str`.
    pub fn as_str(&self) -> &str {
        unsafe { str::from_utf8_unchecked(self.as_bytes()) }
    }

    /// Returns the initialized contents as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.embryo.as_ref().map_or(b"", gc::Embryo::as_slice)
    }

    /// Appends `slice` to the embryo.
    pub fn extend(&mut self, alloc: &mut impl Alloc<'v>, slice: &str) {
        if slice.is_empty() {
            return;
        }
        self.reserve(alloc, slice.len());
        unsafe {
            self.embryo
                .as_mut()
                .unwrap_unchecked()
                .extend(slice.as_bytes())
        }
    }

    /// Replaces `self[start..end]` with `replacement`, shifting the tail in place.
    ///
    /// This does not build a separate staging buffer: existing bytes are shifted within the
    /// embryo's own allocation, growing first only if `replacement` is longer than the range
    /// it replaces.
    ///
    /// # Safety
    /// `start` and `end` must fall on UTF-8 character boundaries, and `replacement` must be
    /// valid UTF-8 (so that the full initialized prefix remains valid UTF-8 afterward).
    ///
    /// # Panics
    /// Panics if `start > end` or `end > self.len()`.
    pub(crate) unsafe fn splice(
        &mut self,
        alloc: &mut impl Alloc<'v>,
        start: usize,
        end: usize,
        replacement: &[u8],
    ) {
        assert!(start <= end && end <= self.len());
        let growth = replacement.len().saturating_sub(end - start);
        self.reserve(alloc, growth);
        unsafe {
            self.embryo
                .as_mut()
                .unwrap_unchecked()
                .splice(start, end, replacement)
        }
    }

    /// Like [`Self::splice`], but does not reserve capacity — the caller must have already
    /// ensured enough spare capacity exists (see [`BinEmbryo::splice_unchecked`] for when
    /// this is needed).
    ///
    /// # Safety
    /// Same requirements as [`Self::splice`], plus: spare capacity must be at least
    /// `replacement.len().saturating_sub(end - start)`.
    pub(crate) unsafe fn splice_unchecked(&mut self, start: usize, end: usize, replacement: &[u8]) {
        if start == end && replacement.is_empty() {
            return;
        }
        assert!(start <= end && end <= self.len());
        unsafe {
            self.embryo
                .as_mut()
                .unwrap_unchecked()
                .splice(start, end, replacement)
        }
    }

    /// Finalizes the embryo into a Do `Str`, writes it to `out`, and resets `self` to an
    /// empty, unallocated embryo, leaving it usable afterward.
    pub(crate) fn freeze_reset(&mut self, alloc: &mut impl Alloc<'v>, out: impl Output<'v>) {
        mem::take(self).finish(alloc, out);
    }

    /// Finalizes the embryo into a Do `Str` and writes it to `out`.
    pub fn finish(self, alloc: &mut impl Alloc<'v>, mut out: impl Output<'v>) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let value = unsafe {
            let header = Header::new(vm.arena(), vm.builtin_types().str.vtbl);
            match self.embryo {
                Some(embryo) => Value::from_object(embryo.finalize_str_unchecked(header)),
                None => {
                    Value::from_object(gc::Base::from_header_utf8_slice(vm.arena(), header, b""))
                }
            }
        };
        Slot::from_output(&mut out).store(value)
    }
}

impl<'v> Format<'v> for StrEmbryo<'v> {
    fn write_str<'s>(&mut self, strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()> {
        self.extend(strand, s);
        Ok(())
    }
}

impl<'v> Format<'v> for BinEmbryo<'v> {
    fn write_str<'s>(&mut self, strand: &mut Strand<'v, 's>, s: &str) -> Result<'v, 's, ()> {
        self.extend(strand, s.as_bytes());
        Ok(())
    }
}

impl<'v> Default for BinEmbryo<'v> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Default for StrEmbryo<'v> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Drop for Value<'v> {
    fn drop(&mut self) {
        unsafe {
            match self.0.decode() {
                Decode::Object(ptr) => mem::drop(Base::from_raw(ptr)),
                Decode::Prim(_) => (),
            }
        }
    }
}

pub struct Weak<'v>(Repr, PhantomData<&'v mut &'v ()>);

impl<'v> Weak<'v> {
    pub(crate) const NIL: Self = Self(Repr::NIL, PhantomData);

    #[expect(dead_code)]
    pub(crate) fn dup(&self) -> Self {
        unsafe {
            match self.0.decode() {
                Decode::Object(base) => base.cast::<arena::Header>().as_ref().retain_weak(),
                Decode::Prim(_) => (),
            }
        }
        Self(self.0.clone(), PhantomData)
    }

    #[expect(dead_code)]
    pub(crate) fn upgrade(&self) -> Option<Value<'v>> {
        if unsafe {
            match self.0.decode() {
                Decode::Object(base) => base.cast::<arena::Header>().as_ref().try_retain(),
                Decode::Prim(_) => true,
            }
        } {
            Some(Value(self.0.clone(), PhantomData))
        } else {
            None
        }
    }

    #[expect(dead_code)]
    pub(crate) fn is_released(&self) -> bool {
        unsafe {
            match self.0.decode() {
                Decode::Object(base) => base.cast::<arena::Header>().as_ref().strong_count() == 0,
                Decode::Prim(_) => false,
            }
        }
    }

    pub(crate) fn from_value(value: &Value<'v>) -> Self {
        match value.case() {
            Case::Object(base) => base.upcast::<arena::Header>().base_get().retain_weak(),
            Case::Prim(_) => (),
        }
        Self(value.0.clone(), PhantomData)
    }

    #[expect(dead_code)]
    pub(crate) fn take(&mut self) -> Self {
        mem::replace(self, Weak::NIL)
    }
}

impl<'v> Drop for Weak<'v> {
    fn drop(&mut self) {
        unsafe {
            match self.0.decode() {
                Decode::Object(ptr) => mem::drop(BaseWeak::from_raw(ptr)),
                Decode::Prim(_) => (),
            }
        }
    }
}

pub(crate) mod private {
    pub struct Sealed;
}

pub(crate) enum InputBy<'v, 'a> {
    Borrow(&'a Value<'v>),
    Value(Value<'v>, Option<Slot<'v, 'a>>),
}

/// Call input parameter.
///
/// This trait abstracts over types that may be passed where a [`Value`] is expected.
pub trait Input<'v> {
    #[allow(private_interfaces)]
    #[doc(hidden)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a>;
}

/// Adapts an async Rust function into a Do callable value.
pub struct AsFunction<F> {
    func: Option<F>,
}

impl<F> AsFunction<F> {
    pub fn new(func: F) -> Self {
        Self { func: Some(func) }
    }
}

impl<'v, F> Input<'v> for AsFunction<F>
where
    F: for<'a, 's> AsyncFn(&mut Strand<'v, 's>, Args<'v, 'a>, Slot<'v, 'a>) -> Result<'v, 's, ()>
        + 'v,
{
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        let func = self.func.take().expect("native function adapter reused");
        let object = GcObj::new_annex(
            vm.arena(),
            vm.builtin_types().native_function,
            NativeFunction::new(func, "<native>", "<function>"),
            NativeFunctionAnnex::default(),
        );
        InputBy::Value(Value::from_object(object), None)
    }
}

/// Adapts an async Rust function into a Do function with bound argument.
///
/// A closure cannot ordinarily capture a `Value`; this permits one to be
/// captured and passed through to the underlying closure.
pub struct AsBoundFunction<B, F> {
    value: Option<B>,
    func: Option<F>,
}

impl<B, F> AsBoundFunction<B, F> {
    pub fn new(func: F, value: B) -> Self {
        Self {
            value: Some(value),
            func: Some(func),
        }
    }
}

impl<'v, B: Input<'v>, F> Input<'v> for AsBoundFunction<B, F>
where
    F: for<'a, 's> AsyncFn(
            &mut Strand<'v, 's>,
            &Value<'v>,
            Args<'v, 'a>,
            Slot<'v, 'a>,
        ) -> Result<'v, 's, ()>
        + 'v,
{
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        let bound = Value::from_input(vm, self.value.take().expect("bound adapter reused"));
        let func = self.func.take().expect("bound adapter reused");
        let object = GcObj::new_annex(
            vm.arena(),
            vm.builtin_types().native_function,
            NativeFunction::bound(func),
            NativeFunctionAnnex {
                bound: UnsafeCell::new(bound),
            },
        );
        InputBy::Value(Value::from_object(object), None)
    }
}

/// Call output parameter.
///
/// This trait abstracts over types that may be passed to receive a [`Value`].
pub trait Output<'v>: Sized {
    #[doc(hidden)]
    fn as_slot(this: &mut Self, _: private::Sealed) -> Slot<'v, '_>;

    /// Set output to input.
    fn set(alloc: &mut impl Alloc<'v>, mut this: Self, input: impl Input<'v>) {
        Output::as_slot(&mut this, private::Sealed).store(Value::from_input(
            alloc.alloc_vm(crate::vm::private::Sealed),
            input,
        ))
    }

    /// Swap with another output.
    fn swap(mut this: Self, mut other: impl Output<'v>) {
        mem::swap(
            Output::as_slot(&mut this, private::Sealed).into_inner(),
            Output::as_slot(&mut other, private::Sealed).into_inner(),
        );
    }
}

/// Value slot.
///
/// This type is spiritually [`&mut Value`](Value), but guarantees:
/// - It's reachable by the garbage collector
/// - Assigning to it passes through any required write barriers
///
/// # Lifetimes
/// - `'v`: VM brand, binding this object to a particular VM
/// - `'a`: scope for which the slot is valid
pub struct Slot<'v, 'a>(*mut Value<'v>, PhantomData<&'a mut Value<'v>>);

impl<'v, 'a> Slot<'v, 'a> {
    pub(crate) fn new(inner: &'a mut Value<'v>) -> Self {
        Self(inner as *mut _, PhantomData)
    }

    pub(crate) fn into_inner(self) -> &'a mut Value<'v> {
        unsafe { &mut *self.0 }
    }

    pub(crate) fn take(&mut self) -> Value<'v> {
        unsafe { mem::replace(&mut *self.0, Value::NIL) }
    }

    pub(crate) fn store(&mut self, value: Value<'v>) {
        unsafe { *self.0 = value }
    }

    pub(crate) fn from_output(this: &'a mut impl Output<'v>) -> Self {
        Output::as_slot(this, private::Sealed)
    }

    /// Reborrows the slot with a (possibly) smaller lifetime, something that happens
    /// automatically with Rust mutable references but sometimes needs
    /// to be done manually here.  Methods that take `impl Output<'v>` will do this
    /// automatically.
    #[inline]
    pub fn reborrow(this: &mut Self) -> Slot<'v, '_> {
        Slot(this.0, PhantomData)
    }
}

impl<'v> Output<'v> for Slot<'v, '_> {
    #[inline]
    fn as_slot(this: &mut Self, _: private::Sealed) -> Slot<'v, '_> {
        Slot::reborrow(this)
    }
}

impl<'v, O: Output<'v>> Output<'v> for &mut O {
    #[inline]
    fn as_slot(this: &mut Self, _: private::Sealed) -> Slot<'v, '_> {
        Output::as_slot(*this, private::Sealed)
    }
}

impl<'v, 'a> Input<'v> for Slot<'v, 'a> {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(self.take(), Some(Slot::reborrow(self)))
    }
}

impl<'v, 'a> Input<'v> for &Slot<'v, 'a> {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Borrow(unsafe { &*self.0 })
    }
}

impl<'v, 'a> Input<'v> for &mut Slot<'v, 'a> {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(self.take(), Some(Slot::reborrow(self)))
    }
}

impl<'v> Input<'v> for &Value<'v> {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Borrow(self)
    }
}

impl<'v> Input<'v> for i128 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self), None)
    }
}

impl<'v> Input<'v> for isize {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for i64 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for i32 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for i16 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for i8 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for usize {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for u64 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for u32 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for u16 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for u8 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_int(vm, *self as i128), None)
    }
}

impl<'v> Input<'v> for f64 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_f64(vm, *self), None)
    }
}

impl<'v> Input<'v> for f32 {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_f64(vm, *self as f64), None)
    }
}

impl<'v> Input<'v> for bool {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_bool(*self), None)
    }
}

impl<'v> Input<'v> for &str {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_str(vm, self), None)
    }
}

impl<'v> Input<'v> for &[u8] {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_u8_slice(vm, self), None)
    }
}

impl<'v, A: Input<'v>, B: Input<'v>> Input<'v> for (A, B) {
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        let first = match self.0.input_take(vm, private::Sealed) {
            InputBy::Borrow(value) => value.dup(),
            InputBy::Value(value, _) => value,
        };
        let second = match self.1.input_take(vm, private::Sealed) {
            InputBy::Borrow(value) => value.dup(),
            InputBy::Value(value, _) => value,
        };
        InputBy::Value(Value::from_object(tuple::tuple(vm, [first, second])), None)
    }
}

/// Input wrapper that creates an immutable tuple from a Rust iterator.
pub struct AsTuple<T>(Option<T>);

impl<T> AsTuple<T> {
    /// Wrap values for conversion into an immutable tuple.
    pub fn new(values: T) -> Self {
        Self(Some(values))
    }
}

impl<'v, T, I> Input<'v> for AsTuple<T>
where
    T: IntoIterator<Item = I>,
    I: Input<'v>,
{
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        let values = self
            .0
            .take()
            .expect("AsTuple input used more than once")
            .into_iter()
            .map(|value| Value::from_input(vm, value))
            .collect::<Vec<_>>();
        InputBy::Value(Value::from_object(tuple::tuple(vm, values)), None)
    }
}

impl<'v, 'a> Deref for Slot<'v, 'a> {
    type Target = Value<'v>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0 }
    }
}

impl<'v, 'a> AsRef<Value<'v>> for Slot<'v, 'a> {
    #[inline]
    fn as_ref(&self) -> &Value<'v> {
        self
    }
}

struct RootInner<'v>(Value<'v>);

unsafe impl<'v> Collect for RootInner<'v> {
    const CYCLIC: bool = true;
    // Mutability is interior rather than going through GC borrow machinery.
    // This is so a `Slot` can exist without a borrow guard
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn arena::Visit) -> ControlFlow<()> {
        self.0.accept(visit)
    }

    fn clear(&mut self) {
        self.0 = Value::NIL
    }
}

/// Garbage collector root.
///
/// This is spiritually a [`Value`], except that it's guaranteed to
/// be reachable by the garbage collector by virtue of being registered
/// as a root.  This makes it potentially more expensive than a [`Slot`].
pub struct Root<'v>(Gc<'v, RootInner<'v>>);

impl<'v> Root<'v> {
    /// Create new root.
    #[inline]
    pub fn new(alloc: &mut impl Alloc<'v>) -> Self {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        Self(Gc::new(vm.arena(), RootInner(Value::NIL)))
    }
}

impl<'v> Output<'v> for &mut Root<'v> {
    #[inline]
    fn as_slot(this: &mut Self, _: private::Sealed) -> Slot<'v, '_> {
        let inner = this.0.try_get_mut().unwrap();
        // Safety: `try_get_mut` only works if the RootInner has exactly 1 strong reference
        // and 1 weak reference, meaning we own it, which should always be the case as we never
        // clone/duplicate the underlying GC smart pointer.
        // The garbage collector can still obtain an mutable/immutable references during cycle walking,
        // which is why `Slot` stores a `*mut Value` rather than an `&mut` and never gives out an
        // `&mut`; it only permits reading (either stealing or `dup`ing the `Value`) or overwriting.
        // This means when the world is stopped for GC, no mutable references should be
        // outstanding.  Furthermore, the GC only obtains a mutable reference on objects that
        // need to be cleared to break cycles, but a root guarantees non-cyclic liveness, so it
        // will only obtain immutable references for child visiting.
        unsafe { Slot::new(&mut (*inner).0) }
    }
}

impl<'v> Deref for Root<'v> {
    type Target = Value<'v>;

    fn deref(&self) -> &Self::Target {
        let inner = self.0.try_get().unwrap();
        // Safety: `try_get` only works if the RootInner has exactly 1 strong reference
        // and 1 weak reference, meaning we own it, which should always be the case as we never
        // clone/duplicate the underlying GC smart pointer.  Because the GC only obtains mutable
        // references on trash objects during cycle collection, and a root guarantees liveness,
        // it will only obtain immutable references which do not conflict with the one we return
        // here.
        unsafe { &(*inner).0 }
    }
}

impl<'v> AsRef<Value<'v>> for Root<'v> {
    fn as_ref(&self) -> &Value<'v> {
        self
    }
}

impl<'v> Input<'v> for &Root<'v> {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        let inner = self.0.try_get().unwrap();
        InputBy::Borrow(unsafe { &(*inner).0 })
    }
}

impl<'v> Input<'v> for &mut Root<'v> {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        let inner = self.0.try_get_mut().unwrap();
        unsafe { InputBy::Value((*inner).0.take(), Some(Slot::new(&mut (*inner).0))) }
    }
}

pub(crate) struct Slots<'v, 'a>(&'a [UnsafeCell<Value<'v>>]);

impl<'v, 'a> Slots<'v, 'a> {
    pub(crate) unsafe fn new(slice: &'a [UnsafeCell<Value<'v>>]) -> Self {
        Self(slice)
    }

    #[expect(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn at<'b>(&'b mut self, index: usize) -> Slot<'v, 'b> {
        unsafe { Slot::new(&mut *self.0[index].get()) }
    }

    pub(crate) fn as_inner(&self) -> &'a [UnsafeCell<Value<'v>>] {
        self.0
    }

    pub(crate) unsafe fn unchecked_at(&self, index: usize) -> Slot<'v, 'a> {
        unsafe { Slot::new(&mut *self.0[index].get()) }
    }
}

/// [`Input`] which becomes a well-known type object
#[non_exhaustive]
pub enum TypeObject {
    /// `std.Value`, the universal supertype
    Value,
    /// `std.Num`, the abstract numeric supertype
    Num,
    /// `std.Type`, the type of types
    Type,
    /// `std.Error`, the supertype of every error type
    Error,
    /// `std.UnsupportedError`
    UnsupportedError,
    /// `std.ImmutableError`
    ImmutableError,
    /// `std.ConcurrencyError`
    ConcurrencyError,
    /// `std.TypeError`
    TypeError,
    /// `std.ValueError`
    ValueError,
    /// `std.StateError`
    StateError,
    /// `std.IndexError`
    IndexError,
    /// `std.FieldError`
    FieldError,
    /// `std.UnexpectedPosError`
    UnexpectedPosError,
    /// `std.UnexpectedKeyError`
    UnexpectedKeyError,
    /// `std.MissingPosError`
    MissingPosError,
    /// `std.MissingKeyError`
    MissingKeyError,
    /// `std.OverflowError`
    OverflowError,
    /// `std.ZeroDivError`
    ZeroDivError,
    /// `std.SinkStop`
    SinkStop,
    /// `std.IterStop`
    IterStop,
    /// `std.CyclicImportError`
    CyclicImportError,
    /// `std.ImportError`
    ImportError,
    /// `std.CompileError`
    CompileError,
    /// `std.RuntimeError`
    RuntimeError,
    /// `std.CanceledError`
    CanceledError,
    /// `std.TimedOutError`
    TimedOutError,
    /// `std.Iter`
    Iter,
    /// `std.Iterable`
    Iterable,
    /// `std.Sink`
    Sink,
    /// `std.Getter`
    Getter,
    /// `std.Setter`
    Setter,
    /// `std.FmtValue`, a value bound to a format specification
    FmtValue,
    /// `std.FmtParam`, an unbound position in a sequence
    FmtParam,
    /// `std.Fmt`, a sequence of literal text, bound interpolations, and
    /// unbound parameters
    Fmt,
}

impl<'v> Input<'v> for TypeObject {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        let builtins = vm.singletons();
        InputBy::Borrow(match self {
            TypeObject::Value => &builtins.value,
            TypeObject::Num => &builtins.num,
            TypeObject::Type => &builtins.type_obj,
            TypeObject::Error => &builtins.error,
            TypeObject::UnsupportedError => &builtins.error_unsupported,
            TypeObject::ImmutableError => &builtins.error_immutable,
            TypeObject::ConcurrencyError => &builtins.error_concurrency,
            TypeObject::TypeError => &builtins.error_type,
            TypeObject::ValueError => &builtins.error_value,
            TypeObject::StateError => &builtins.error_state,
            TypeObject::IndexError => &builtins.error_index,
            TypeObject::FieldError => &builtins.error_field,
            TypeObject::UnexpectedPosError => &builtins.error_unexpected_pos,
            TypeObject::UnexpectedKeyError => &builtins.error_unexpected_key,
            TypeObject::MissingPosError => &builtins.error_missing_pos,
            TypeObject::MissingKeyError => &builtins.error_missing_key,
            TypeObject::OverflowError => &builtins.error_overflow,
            TypeObject::ZeroDivError => &builtins.error_zerodiv,
            TypeObject::SinkStop => &builtins.error_sink_stop,
            TypeObject::IterStop => &builtins.error_iter_stop,
            TypeObject::CyclicImportError => &builtins.error_cyclic_import,
            TypeObject::ImportError => &builtins.error_import,
            TypeObject::CompileError => &builtins.error_compile,
            TypeObject::RuntimeError => &builtins.error_runtime,
            TypeObject::CanceledError => &builtins.error_canceled,
            TypeObject::TimedOutError => &builtins.error_timed_out,
            TypeObject::Iter => &builtins.input_iter,
            TypeObject::Iterable => &builtins.iterable,
            TypeObject::Sink => &builtins.output_iter,
            TypeObject::Getter => &builtins.getter,
            TypeObject::Setter => &builtins.setter,
            TypeObject::FmtValue => crate::stdlib::fmt::fmt_value_singleton(vm),
            TypeObject::FmtParam => crate::stdlib::fmt::fmt_param_singleton(vm),
            TypeObject::Fmt => crate::stdlib::fmt::fmt_singleton(vm),
        })
    }
}

/// [`Input`] which becomes the `nil` value
pub struct Nil;

impl<'v> Input<'v> for Nil {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'a>(&'a mut self, _vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        InputBy::Value(Value::NIL, None)
    }
}

/// [`Input`] which becomes a new empty builtin type
#[non_exhaustive]
pub enum Empty {
    /// Empty array
    Array,
    /// Empty dict
    Dict,
    /// Empty set
    Set,
}

impl<'v> Input<'v> for Empty {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        InputBy::Value(
            match self {
                Empty::Array => Value::from_object(GcObj::new(
                    vm.arena(),
                    vm.builtin_types().array,
                    array::Array::new(),
                )),
                Empty::Dict => Value::from_object(GcObj::new(
                    vm.arena(),
                    vm.builtin_types().dict,
                    dict::Dict::new(),
                )),
                Empty::Set => Value::from_object(GcObj::new(
                    vm.arena(),
                    vm.builtin_types().set,
                    set::Set::new(),
                )),
            },
            None,
        )
    }
}

/// [`Input`] which becomes a well-known singleton object
#[non_exhaustive]
pub enum Singleton {
    /// `std.null`
    Null,
}

impl<'v> Input<'v> for Singleton {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: private::Sealed) -> InputBy<'v, 'a> {
        InputBy::Borrow(match self {
            Singleton::Null => &vm.singletons().null,
        })
    }
}
