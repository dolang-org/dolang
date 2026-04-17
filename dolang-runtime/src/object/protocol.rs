use std::{
    fmt,
    hash::{DefaultHasher, Hasher},
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::{
    arg::Args,
    bytecode::Variadic,
    error::{Error, Result},
    gc::{
        self, Boxable, Boxed, Collect,
        arena::{self, Arena, Upcast},
    },
    object::class::get_native_slot,
    sig::Unpack,
    strand::{Pinned, Strand},
    sym::{self, Sym},
    unpack,
    value::{Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::Vm,
};

pub(crate) struct Inspect<'v, 'a> {
    pub(crate) is_abstract: bool,
    pub(crate) members: Vec<Sym<'v, 'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpreadContext {
    Args,
    Sequence,
    Pairs,
}

pub trait Spread<'v, 's> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;
}

pub(crate) async fn default_spread<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: impl Input<'v>,
    context: SpreadContext,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    use std::cell::UnsafeCell;

    strand
        .with_slots(
            async move |strand, [mut root, mut iter, mut item, mut key, mut val]| {
                Output::set(strand, Slot::reborrow(&mut root), value);
                root.op_iter(strand, Slot::reborrow(&mut iter)).await?;
                match context {
                    SpreadContext::Sequence | SpreadContext::Args => {
                        while iter.op_next(strand, Slot::reborrow(&mut item)).await? {
                            sink.positional(strand, Slot::reborrow(&mut item))?;
                        }
                    }
                    SpreadContext::Pairs => {
                        while iter.op_next(strand, Slot::reborrow(&mut item)).await? {
                            let unpack = Unpack {
                                required: 2,
                                optional: vec![],
                                keys: vec![],
                                sym_index: vec![],
                                variadic: Variadic::None,
                            };
                            let cells = [UnsafeCell::new(Value::NIL), UnsafeCell::new(Value::NIL)];
                            item.op_unpack(strand, &unpack, unsafe { Slots::new(&cells) })
                                .await?;
                            key.store(unsafe { (*cells[0].get()).take() });
                            val.store(unsafe { (*cells[1].get()).take() });
                            sink.keyed(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut val))?;
                        }
                    }
                }
                Ok(())
            },
        )
        .await
}

pub(crate) trait Protocol<'v>: Boxable<Header> + Collect + 'v {
    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "call not supported"))
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(async move |strand, [mut func]| {
                Self::op_get(this, strand, method, Slot::reborrow(&mut func))?;
                func.op_call(strand, args, out).await
            })
            .await
    }

    /// Like [`op_mcall`], but called when this object received the call via class delegation.
    ///
    /// `delegator` is the original value (e.g. a `ClassInstance`) that delegated the call to
    /// this native object.  The default implementation ignores the delegator and calls
    /// [`op_mcall`] unchanged, preserving the existing behaviour for all types that do not
    /// care about the caller's identity.  Types that implement mixin methods (methods that
    /// operate on the delegator rather than on the native inner object) should override this.
    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let _ = delegator;
        Self::op_mcall(this, strand, method, args, out).await
    }

    fn op_type<'a, 's>(this: Recv<'v, 'a, Self>, strand: &'a mut Strand<'v, 's>, out: Slot<'v, 'a>);

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this) || supertype.eq(strand, TypeObject::Value)
    }

    #[allow(unused_variables)]
    fn op_inspect<'a>(this: Recv<'v, 'a, Self>, vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        None
    }

    /// Fill a native slot on this object identified by `type_obj` with `native`.
    ///
    /// Called during class-instance construction to register a native super object in the
    /// appropriate slot.  The default implementation returns a type error; only
    /// [`ClassInstance`](crate::object::class::ClassInstance) provides a meaningful
    /// implementation.
    #[allow(unused_variables)]
    fn op_fill<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "fill not supported"))
    }

    fn op_display_arg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_display(this, strand, w)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()>;

    fn op_bool<'a, 's>(_this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        true
    }

    fn op_eq<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_ne<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(
            !Self::op_eq(this, strand, other)?.op_bool(strand),
        ))
    }

    fn op_neg<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::type_error(strand, "negation not supported"))
    }

    fn op_bnot<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::type_error(strand, "bitwise inverse not supported"))
    }

    fn op_band<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_bor<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_bxor<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_add<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_sub<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_rsub<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_mul<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_div<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_rdiv<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_ediv<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_rediv<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_mod<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_rmod<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_lt<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_lte<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(
            Self::op_lt(this.clone(), strand, other)?.op_bool(strand)
                || Self::op_eq(this, strand, other)?.op_bool(strand),
        ))
    }

    fn op_gt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(
            !Self::op_lte(this, strand, other)?.op_bool(strand),
        ))
    }

    fn op_gte<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(
            !Self::op_lt(this, strand, other)?.op_bool(strand),
        ))
    }

    fn op_get<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _field: Sym<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "field get not supported"))
    }

    fn op_set<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _field: Sym<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "field set not supported"))
    }

    fn op_index<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _index: &Value<'v>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "indexing not supported"))
    }

    fn op_assign<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _index: Slot<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "index assignment not supported"))
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        ptr::hash(gc::Borrow::into_raw(this.receiver).as_ptr(), hasher);
        Ok(())
    }

    async fn op_next<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Err(Error::type_error(
            strand,
            "iteration protocol not supported",
        ))
    }

    async fn op_put<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _item: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "sink protocol not supported"))
    }

    async fn op_iter<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            "iteration protocol not supported",
        ))
    }

    async fn op_sink<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "sink protocol not supported"))
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        default_spread(strand, this.clone(), context, sink).await
    }

    #[allow(unused_variables)]
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::not_supported(strand))
    }
}

#[derive(Clone, Copy)]
enum BinOp {
    Eq,
    Ne,
    Band,
    Bor,
    Bxor,
    Add,
    Sub,
    Rsub,
    Mul,
    Div,
    Rdiv,
    Ediv,
    Rediv,
    Mod,
    Rmod,
}

#[derive(Clone, Copy)]
enum FmtOp {
    DisplayArg,
    Display,
    Debug,
}

#[derive(Clone, Copy)]
enum UnaryOp {
    Neg,
    Bnot,
}

#[derive(Clone, Copy)]
enum CmpOp {
    Lt,
    Lte,
    Gt,
    Gte,
}

/// Virtual method table for object protocol dispatch.
///
/// ## Vtable Structure
///
/// Each object type has a vtable containing function pointers for all protocol operations. This
/// enables polymorphic dispatch without (fat) dynamic trait objects.  A series of glue functions
/// bridges each entry to a method in the `Protocol` trait.  The `Dispatch` trait and its blanket
/// impl provides a safe wrapper to invoke the operations.
///
/// The vtable is split into two parts:
/// - `base`: GC-related operations (drop, trace, etc.) from `arena::Vtbl`
/// - Protocol methods: Type-specific operations (call, get, set, etc.)
///
/// ## Lifetime Management
///
/// The `'v` lifetime parameter ensures that values returned by methods are tied
/// to the VM's lifetime. The `&'a &'v ()` parameter in each function creates an
/// implied `'v: 'a` bound that can't be expressed explicitly in the type system.
///
/// ## Invariance
///
/// The `PhantomData` field ensures `'v` is treated as invariant.
/// This is necessary for soundness with the GC - we can't allow covariance
/// that would permit unsound lifetime extensions.
///
/// ## Type Safety
///
/// `Vtbl` is not parameterized by the concrete type `T` it was created for. Instead,
/// [`TypeHandle<'v, T>`] wraps a `&'v Vtbl<'v>` with a phantom `T` to witness the erased
/// type, enabling safe downcasts via pointer identity checks.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Vtbl<'v> {
    /// Base vtable for GC operations
    base: arena::Vtbl,
    /// Ensure 'v is invariant.
    /// This prevents "mixing" of objects from different GC arenas.
    phantom: PhantomData<&'v mut &'v ()>,
    op_type: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        // This introduces an implied 'v: 'a bound which can't be expressed explicitly
        _: &'a &'v (),
    ),
    op_subtype: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
        _: &'a &'v (),
    ) -> bool,
    op_inspect:
        for<'a> fn(this: NonNull<Header>, vm: &Vm<'v>, _: &'a &'v ()) -> Option<Inspect<'v, 'a>>,
    op_fill: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_call: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    #[expect(clippy::type_complexity)]
    op_mcall: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        delegator: Option<&'a Value<'v>>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    #[expect(clippy::type_complexity)]
    op_fmt: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        op: FmtOp,
        w: &mut dyn fmt::Write,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_bool: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        _: &'a &'v (),
    ) -> bool,
    op_unary: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        op: UnaryOp,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_bin: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        op: BinOp,
        other: &Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_cmp: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        op: CmpOp,
        other: &Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_get: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_set: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_index: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_assign: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_hash: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_next: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, bool>,
    op_put: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_iter: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_sink: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    #[expect(clippy::type_complexity)]
    op_spread: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_unpack: for<'a, 's> fn(
        this: NonNull<Header>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
}

impl<'v> Vtbl<'v> {
    pub(crate) fn new<T: ?Sized + Boxable<Header> + Protocol<'v>>() -> Self {
        Self {
            base: *Boxed::<Header, T>::vtbl(),
            phantom: PhantomData,
            op_type: op_type_glue::<T>,
            op_subtype: op_subtype_glue::<T>,
            op_inspect: op_inspect_glue::<T>,
            op_fill: op_fill_glue::<T>,
            op_call: op_call_glue::<T>,
            op_mcall: op_mcall_glue::<T>,
            op_fmt: op_fmt_glue::<T>,
            op_bool: to_bool_glue::<T>,
            op_unary: op_unary_glue::<T>,
            op_bin: op_bin_glue::<T>,
            op_cmp: op_cmp_glue::<T>,
            op_get: op_get_glue::<T>,
            op_set: op_set_glue::<T>,
            op_index: op_index_glue::<T>,
            op_assign: op_assign_glue::<T>,
            op_hash: op_hash_glue::<T>,
            op_next: op_next_glue::<T>,
            op_put: op_put_glue::<T>,
            op_iter: op_iter_glue::<T>,
            op_sink: op_sink_glue::<T>,
            op_spread: op_spread_glue::<T>,
            op_unpack: op_unpack_glue::<T>,
        }
    }
}

unsafe impl<'v> Upcast<arena::Vtbl> for Vtbl<'v> {}

/// Type-safe handle to a registered [`Vtbl`].
///
/// The phantom `T` witnesses which concrete type the vtbl was created for,
/// enabling safe downcasts via pointer identity checks. This replaces the
/// former type parameter on `Vtbl` itself, keeping the vtbl struct concrete
/// while maintaining type safety at API boundaries.
pub(crate) struct TypeHandle<'v, T: ?Sized + 'v> {
    pub(crate) vtbl: NonNull<Vtbl<'v>>,
    _phantom: PhantomData<(&'v Vtbl<'v>, T)>,
}

impl<'v, T: ?Sized + 'v> Copy for TypeHandle<'v, T> {}

impl<'v, T: ?Sized + 'v> Clone for TypeHandle<'v, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, T: ?Sized + 'v> TypeHandle<'v, T> {
    /// Create a new type handle from a vtbl reference.
    ///
    /// # Safety
    ///
    /// The vtbl must have been created via `Vtbl::new::<T>()`.
    pub(crate) unsafe fn new(vtbl: NonNull<Vtbl<'v>>) -> Self {
        Self {
            vtbl,
            _phantom: PhantomData,
        }
    }

    pub(crate) fn vtbl(self) -> &'v Vtbl<'v> {
        unsafe { self.vtbl.as_ref() }
    }
}

#[repr(C)]
pub(crate) struct Header {
    base: arena::Header,
}

unsafe impl Upcast<Header> for Header {}
unsafe impl<T: Upcast<Header>> Upcast<arena::Header> for T {}

impl Header {
    pub(crate) unsafe fn vtbl<'v>(&self) -> &Vtbl<'v> {
        unsafe { self.base.vtbl_downcast_unchecked::<Vtbl<'v>>() }
    }
    pub(crate) unsafe fn vtbl_downcast_unchecked<V: Upcast<arena::Vtbl>>(&self) -> &V {
        unsafe { self.base.vtbl_downcast_unchecked::<V>() }
    }
    /// # Safety
    ///
    /// The vtbl must have been created for the type that will be stored behind this header.
    pub(crate) unsafe fn new<'v>(arena: &Arena<'v>, vtbl: NonNull<Vtbl<'v>>) -> Self {
        Self {
            base: unsafe { arena::Header::new(arena, vtbl) },
        }
    }
}

pub(crate) type Ref<'v, 'a, T> = gc::Ref<'v, 'a, Header, T>;

pub(crate) type Mut<'v, 'a, T> = gc::Mut<'v, 'a, Header, T>;

/// `this` type received by object protocol methods.
/// Allows borrowing the underlying `T` or obtaining a strong reference.
pub(crate) struct Recv<'v, 'a, T: ?Sized + Boxable<Header>> {
    pub(crate) receiver: GcObjBorrow<'v, 'a, T>,
}

impl<'v, 'a, T: ?Sized + Boxable<Header>> Clone for Recv<'v, 'a, T> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver,
        }
    }
}

impl<'v, 'a, T: ?Sized + Boxable<Header>> Recv<'v, 'a, T> {
    pub(crate) fn new(receiver: gc::Borrow<'v, 'a, Header, T>) -> Self {
        Self { receiver }
    }

    unsafe fn from_header(header: NonNull<Header>) -> Self {
        unsafe {
            Self {
                receiver: gc::Borrow::from_raw(header.cast()),
            }
        }
    }

    pub(crate) fn as_header(&self) -> NonNull<Header> {
        self.receiver.as_header()
    }

    pub(crate) unsafe fn vtbl_downcast_unchecked<V: Upcast<arena::Vtbl>>(&self) -> &V {
        unsafe {
            self.receiver
                .as_header()
                .as_ref()
                .vtbl_downcast_unchecked::<V>()
        }
    }

    pub(crate) fn get(&self) -> &T
    where
        T: gc::Collect,
    {
        self.receiver.get()
    }

    pub(crate) fn borrow<'s>(
        &'a self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Ref<'v, 'a, T>> {
        self.receiver
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))
    }

    pub(crate) fn borrow_mut<'s>(
        &'a self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Mut<'v, 'a, T>> {
        self.receiver
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))
    }

    pub(crate) fn to_strong(&self) -> GcObj<'v, T>
    where
        <T as Boxable<Header>>::Inner: Upcast<Header>,
    {
        self.receiver.to_strong()
    }
}

impl<'v, 'a, T: Protocol<'v> + Boxable<Header, Inner = gc::BoxedSized<Header, T>>> Recv<'v, 'a, T> {
    pub(crate) fn annex(&self) -> &'a T::Annex {
        self.receiver.annex()
    }
}

impl<'v, 'a, T: ?Sized + Protocol<'v>> Input<'v> for Recv<'v, 'a, T> {
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_object(self.receiver.to_strong()), None)
    }
}

impl<'v, 'a, T: ?Sized + Protocol<'v>> Input<'v> for &Recv<'v, 'a, T> {
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_object(self.receiver.to_strong()), None)
    }
}

fn op_type_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) {
    unsafe { T::op_type(Recv::from_header(this), strand, out) }
}

fn op_subtype_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    supertype: &Value<'v>,
    _: &'a &'v (),
) -> bool {
    unsafe { T::op_subtype(Recv::from_header(this), strand, supertype) }
}

fn op_inspect_glue<'v, 'a, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    vm: &Vm<'v>,
    _: &'a &'v (),
) -> Option<Inspect<'v, 'a>> {
    unsafe { T::op_inspect(Recv::from_header(this), vm) }
}

fn op_fill_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    type_obj: &Value<'v>,
    native: Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_fill(Recv::from_header(this), strand, type_obj, native) }
}

fn op_call_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_call(Recv::from_header(this), strand, args, out).await
        })
    }
}

fn op_mcall_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    delegator: Option<&'a Value<'v>>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| match delegator {
            Some(delegator) => {
                T::op_dcall(
                    Recv::from_header(this),
                    strand,
                    delegator,
                    method,
                    args,
                    out,
                )
                .await
            }
            None => T::op_mcall(Recv::from_header(this), strand, method, args, out).await,
        })
    }
}

fn op_fmt_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    op: FmtOp,
    w: &mut dyn fmt::Write,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe {
        let this = Recv::from_header(this);
        match op {
            FmtOp::DisplayArg => T::op_display_arg(this, strand, w),
            FmtOp::Display => T::op_display(this, strand, w),
            FmtOp::Debug => T::op_debug(this, strand, w),
        }
    }
}

fn to_bool_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    _: &'a &'v (),
) -> bool {
    unsafe { T::op_bool(Recv::from_header(this), strand) }
}

fn op_unary_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    op: UnaryOp,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_header(this);
        match op {
            UnaryOp::Neg => T::op_neg(this, strand),
            UnaryOp::Bnot => T::op_bnot(this, strand),
        }
    }
}

fn op_bin_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    op: BinOp,
    other: &'a Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_header(this);
        match op {
            BinOp::Eq => T::op_eq(this, strand, other),
            BinOp::Ne => T::op_ne(this, strand, other),
            BinOp::Band => T::op_band(this, strand, other),
            BinOp::Bor => T::op_bor(this, strand, other),
            BinOp::Bxor => T::op_bxor(this, strand, other),
            BinOp::Add => T::op_add(this, strand, other),
            BinOp::Sub => T::op_sub(this, strand, other),
            BinOp::Rsub => T::op_rsub(this, strand, other),
            BinOp::Mul => T::op_mul(this, strand, other),
            BinOp::Div => T::op_div(this, strand, other),
            BinOp::Rdiv => T::op_rdiv(this, strand, other),
            BinOp::Ediv => T::op_ediv(this, strand, other),
            BinOp::Rediv => T::op_rediv(this, strand, other),
            BinOp::Mod => T::op_mod(this, strand, other),
            BinOp::Rmod => T::op_rmod(this, strand, other),
        }
    }
}

fn op_cmp_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    op: CmpOp,
    other: &'a Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_header(this);
        match op {
            CmpOp::Lt => T::op_lt(this, strand, other),
            CmpOp::Lte => T::op_lte(this, strand, other),
            CmpOp::Gt => T::op_gt(this, strand, other),
            CmpOp::Gte => T::op_gte(this, strand, other),
        }
    }
}

fn op_get_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_get(Recv::from_header(this), strand, field, out) }
}

fn op_set_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    field: Sym<'v, 'a>,
    value: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_set(Recv::from_header(this), strand, field, value) }
}

fn op_index_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    index: &Value<'v>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_index(Recv::from_header(this), strand, index, out) }
}

fn op_assign_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    index: Slot<'v, 'a>,
    value: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_assign(Recv::from_header(this), strand, index, value) }
}

fn op_hash_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    hasher: &mut DefaultHasher,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_hash(Recv::from_header(this), strand, hasher) }
}

fn op_next_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, bool> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_next(Recv::from_header(this), strand, out).await
        })
    }
}

fn op_put_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    item: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_put(Recv::from_header(this), strand, item).await
        })
    }
}

fn op_iter_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_iter(Recv::from_header(this), strand, out).await
        })
    }
}

fn op_sink_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_sink(Recv::from_header(this), strand, out).await
        })
    }
}

fn op_spread_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    context: SpreadContext,
    sink: &'a mut dyn Spread<'v, 's>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_spread(Recv::from_header(this), strand, context, sink).await
        })
    }
}

fn op_unpack_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: NonNull<Header>,
    strand: &'a mut Strand<'v, 's>,
    sig: &'a Unpack<'v, 'a>,
    out: Slots<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_unpack(Recv::from_header(this), strand, sig, out).await
        })
    }
}

pub(crate) type GcObj<'v, T> = gc::Box<'v, Header, T>;
pub(crate) type GcObjBorrow<'v, 'a, T> = gc::Borrow<'v, 'a, Header, T>;
#[expect(dead_code)]
pub(crate) type WeakObj<'v, T> = gc::BoxWeak<'v, Header, T>;

impl<'v, 'a, T: gc::Collect> AsHeader for GcObjBorrow<'v, 'a, T> {
    unsafe fn as_header(&self) -> NonNull<Header> {
        self.into_raw().cast()
    }
}

impl<'v, 'a> AsHeader for gc::BaseBorrow<'v, 'a, Header> {
    unsafe fn as_header(&self) -> NonNull<Header> {
        self.into_raw()
    }
}

impl<'v, T: Protocol<'v>> GcObj<'v, T> {
    pub(crate) fn new(arena: &Arena<'v>, handle: TypeHandle<'v, T>, value: T) -> Self
    where
        T::Annex: Default,
    {
        unsafe { gc::Base::from_parts(arena, Header::new(arena, handle.vtbl), value) }
    }

    pub(crate) fn new_annex(
        arena: &Arena<'v>,
        handle: TypeHandle<'v, T>,
        value: T,
        annex: T::Annex,
    ) -> Self {
        unsafe { gc::Base::from_parts_annex(arena, Header::new(arena, handle.vtbl), value, annex) }
    }
}

/// Macro for invoking vtable methods on GC objects.
///
/// ## Dispatch Process
///
/// This macro performs vtable dispatch in three steps:
/// 1. Get the header pointer: `$obj.as_inner()`
/// 2. Extract the vtable: `this.as_ref().vtbl::<()>`
/// 3. Call the method: `(vtbl.$meth)(this, $strand, ...args, &&())`
///
/// ## The `&&()` Parameter
///
/// Each vtable method takes a final `&&()` parameter. This is a hack to express the lifetime bound
/// `'v: 'a` (the VM outlives all other references) which cannot be written explicitly for raw `fn`
/// types in Rust.
///
/// # Safety
///
/// This macro must be invoked within an `unsafe` block because it calls unsafe functions
/// from the vtable which cast and dereference object header pointers.
macro_rules! invoke {
    ($obj: expr, $meth: ident, $strand: expr) => {
        {
            let this = $obj.as_header();
            let vtbl = this.as_ref().vtbl();
            (vtbl.$meth)(this, $strand, &&())
        }
    };
    ($obj: expr, $meth: ident, $strand: expr, $($params: expr),+) => {
        {
            let this = $obj.as_header();
            let vtbl = this.as_ref().vtbl();
            (vtbl.$meth)(this, $strand, $($params),*, &&())
        }
    };
}

pub(crate) trait AsHeader {
    unsafe fn as_header(&self) -> NonNull<Header>;
}

/// Trait for dispatching protocol operations on objects.
///
/// ## Dispatch Mechanism
///
/// This trait provides a uniform interface for calling object protocol methods
/// regardless of the object's concrete type. It's implemented for any type that
/// wraps an object header (via `AsHeader`).
///
/// The implementation uses vtable dispatch through the `invoke!` macro:
/// 1. Get the object's header pointer via `as_header()`
/// 2. Extract the vtable from the header
/// 3. Call the appropriate vtable method
pub(crate) trait Dispatch<'v, 'a> {
    async fn op_call<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    async fn op_mcall<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    async fn op_dcall<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    fn op_fill<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_type<'s>(&self, strand: &'a mut Strand<'v, 's>, out: Slot<'v, 'a>);

    fn op_subtype<'s>(&self, strand: &'a mut Strand<'v, 's>, supertype: &Value<'v>) -> bool;

    fn op_inspect(&self, vm: &Vm<'v>) -> Option<Inspect<'v, 'a>>;

    fn op_display_arg<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()>;

    fn op_display<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()>;

    fn op_debug<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()>;

    fn op_bool<'s>(&self, strand: &'a mut Strand<'v, 's>) -> bool;

    fn op_bnot<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>>;

    fn op_neg<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>>;

    fn op_band<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_bor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_bxor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_add<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_sub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_rsub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_mul<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_div<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_rdiv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_ediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_rediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_mod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_rmod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_eq<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_ne<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_lt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_lte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_gt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_gte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_get<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    fn op_set<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    fn op_index<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>;

    fn op_assign<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    fn op_hash<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()>;

    async fn op_next<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool>;

    async fn op_put<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    async fn op_iter<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    async fn op_sink<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;

    async fn op_spread<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()>;

    async fn op_unpack<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()>;
}

impl<'v, 'a, T: AsHeader> Dispatch<'v, 'a> for T {
    fn op_call<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        unsafe { invoke!(self, op_call, strand, args, out) }
    }

    fn op_mcall<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        unsafe { invoke!(self, op_mcall, strand, None, method, args, out) }
    }

    fn op_dcall<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        unsafe { invoke!(self, op_mcall, strand, Some(delegator), method, args, out) }
    }

    fn op_fill<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_fill, strand, type_obj, native) }
    }

    fn op_type<'s>(&self, strand: &'a mut Strand<'v, 's>, out: Slot<'v, 'a>) {
        unsafe { invoke!(self, op_type, strand, out) }
    }

    fn op_subtype<'s>(&self, strand: &'a mut Strand<'v, 's>, supertype: &Value<'v>) -> bool {
        unsafe { invoke!(self, op_subtype, strand, supertype) }
    }

    fn op_inspect(&self, vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        unsafe { invoke!(self, op_inspect, vm) }
    }

    fn op_display_arg<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_fmt, strand, FmtOp::DisplayArg, w) }
    }

    fn op_display<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_fmt, strand, FmtOp::Display, w) }
    }

    fn op_debug<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_fmt, strand, FmtOp::Debug, w) }
    }

    fn op_bool<'s>(&self, strand: &'a mut Strand<'v, 's>) -> bool {
        unsafe { invoke!(self, op_bool, strand) }
    }

    fn op_eq<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Eq, other) }
    }

    fn op_ne<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Ne, other) }
    }

    fn op_neg<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_unary, strand, UnaryOp::Neg) }
    }

    fn op_bnot<'s>(&self, strand: &'a mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_unary, strand, UnaryOp::Bnot) }
    }

    fn op_band<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Band, other) }
    }

    fn op_bor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Bor, other) }
    }

    fn op_bxor<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Bxor, other) }
    }

    fn op_add<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Add, other) }
    }

    fn op_sub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Sub, other) }
    }

    fn op_rsub<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Rsub, other) }
    }

    fn op_mul<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Mul, other) }
    }

    fn op_div<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Div, other) }
    }

    fn op_rdiv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Rdiv, other) }
    }

    fn op_ediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Ediv, other) }
    }

    fn op_rediv<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Rediv, other) }
    }

    fn op_mod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Mod, other) }
    }

    fn op_rmod<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Rmod, other) }
    }

    fn op_lt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_cmp, strand, CmpOp::Lt, other) }
    }

    fn op_lte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_cmp, strand, CmpOp::Lte, other) }
    }

    fn op_gt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_cmp, strand, CmpOp::Gt, other) }
    }

    fn op_gte<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_cmp, strand, CmpOp::Gte, other) }
    }

    fn op_get<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_get, strand, field, out) }
    }

    fn op_set<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_set, strand, field, value) }
    }

    fn op_index<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_index, strand, index, out) }
    }

    fn op_assign<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_assign, strand, index, value) }
    }

    fn op_hash<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_hash, strand, hasher) }
    }

    async fn op_next<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        unsafe { invoke!(self, op_next, strand, out) }.await
    }

    async fn op_put<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_put, strand, item) }.await
    }

    async fn op_iter<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_iter, strand, out) }.await
    }

    async fn op_sink<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_sink, strand, out) }.await
    }

    async fn op_spread<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_spread, strand, context, sink) }.await
    }

    async fn op_unpack<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_unpack, strand, sig, out) }.await
    }
}

/// Dispatch a type-object method call, converting from explicit-self calling convention to
/// normal receiver convention.
///
/// `self_val` is the instance (first positional argument of the type-object call).  All
/// `ClassInstance` delegation is already handled by the `Value`-level operations, so this
/// function simply dispatches to the appropriate `Value` op.
///
/// Protocol-level special method symbols are shimmed to the corresponding `Value`-level
/// operation.  All other symbols are forwarded to `self_val.op_mcall(strand, method, trailing, out)`.
pub(crate) async fn dispatch_native_method<'v, 's>(
    strand: &mut Strand<'v, 's>,
    ty: &Value<'v>,
    method: Sym<'v, '_>,
    args: Args<'v, '_>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let ([this], [], trailing) = unpack!(strand, args, 1, 0, ...)?;
    let this = if let Some(inst) = this.downcast_ref(strand.builtin_types().class_instance) {
        get_native_slot(strand, inst, ty)
            .ok_or_else(|| Error::type_error(strand, "not a native object subclass"))?
    } else {
        strand.with_slots_sync(|strand, [mut tmp]| {
            this.op_type(strand, Slot::reborrow(&mut tmp));
            if !tmp.repr_eq(strand, ty) {
                return Err(Error::type_error(strand, "invalid native object type"));
            }
            Ok(&this)
        })?
    };
    match method.tag() {
        sym::STR_METHOD => {
            let s = this.to_string(strand)?;
            Output::set(strand, out, s.as_str());
        }
        sym::DBG_METHOD => {
            let s = this.to_debug(strand)?;
            Output::set(strand, out, s.as_str());
        }
        sym::BOOL_METHOD => {
            let b = this.op_bool(strand);
            Output::set(strand, out, b);
        }
        sym::HASH_METHOD => {
            let mut hasher = DefaultHasher::new();
            this.op_hash(strand, &mut hasher)?;
            Output::set(strand, out, hasher.finish() as i64);
        }
        sym::EQ_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_eq(strand, &other));
        }
        sym::LT_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_lt(strand, &other)?);
        }
        sym::NEG_METHOD => {
            out.store(this.op_neg(strand)?);
        }
        sym::BNOT_METHOD => {
            out.store(this.op_bnot(strand)?);
        }
        sym::ADD_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_add(strand, &other)?);
        }
        sym::SUB_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_sub(strand, &other)?);
        }
        sym::RSUB_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_rsub(strand, &other)?);
        }
        sym::MUL_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_mul(strand, &other)?);
        }
        sym::DIV_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_div(strand, &other)?);
        }
        sym::RDIV_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_rdiv(strand, &other)?);
        }
        sym::EDIV_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_ediv(strand, &other)?);
        }
        sym::REDIV_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_rediv(strand, &other)?);
        }
        sym::MOD_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_mod(strand, &other)?);
        }
        sym::RMOD_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_rmod(strand, &other)?);
        }
        sym::BAND_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_band(strand, &other)?);
        }
        sym::BOR_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_bor(strand, &other)?);
        }
        sym::BXOR_METHOD => {
            let ([other], []) = unpack!(strand, trailing, 1, 0)?;
            out.store(this.op_bxor(strand, &other)?);
        }
        _ => {
            this.op_mcall(strand, method, trailing, out).await?;
        }
    }
    Ok(())
}
