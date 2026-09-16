use std::{
    hash::{DefaultHasher, Hasher},
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::value::fmt::{Format, Spec};

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{
        self, Boxable, Boxed, Collect,
        arena::{self, Arena, Upcast},
    },
    object::{BoundMethod, class::get_native_slot},
    sig::Unpack,
    strand::{Pinned, Strand},
    sym::{self, Sym},
    unpack,
    value::{Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::{Alloc, Vm},
};

pub(crate) struct Inspect<'v, 'a> {
    pub(crate) is_abstract: bool,
    pub(crate) members: &'a [Member<'v, 'a>],
    pub(crate) type_members: &'a [Member<'v, 'a>],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemberKind {
    Method,
    Getter,
    Setter,
    Property,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Member<'v, 'a> {
    pub(crate) sym: Sym<'v, 'a>,
    pub(crate) kind: MemberKind,
}

impl<'v, 'a> Member<'v, 'a> {
    pub(crate) const fn new(sym: Sym<'v, 'a>, kind: MemberKind) -> Self {
        Self { sym, kind }
    }

    pub(crate) const fn method(sym: Sym<'v, 'a>) -> Self {
        Self::new(sym, MemberKind::Method)
    }

    pub(crate) const fn coerce_static_slice<'b>(
        slice: &'b [Member<'static, 'static>],
    ) -> &'b [Self] {
        // SAFETY: a static symbol must be well-known, and well-known symbols
        // have the same identity in every VM.
        unsafe { std::mem::transmute(slice) }
    }
}

macro_rules! members {
    ($($kind:ident($tag:expr)),* $(,)?) => {{
        const MEMBERS: &[$crate::object::protocol::Member<'static, 'static>] = &[
            $($crate::object::protocol::Member::new(
                $crate::sym::Sym::well_known($tag),
                $crate::object::protocol::MemberKind::$kind,
            )),*
        ];
        $crate::object::protocol::Member::coerce_static_slice(MEMBERS)
    }};
}

pub(crate) use members;

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
    strand
        .with_slots(async move |strand, [mut root, mut iter, mut item]| {
            Output::set(strand, Slot::reborrow(&mut root), value);
            root.op_iter(strand, Slot::reborrow(&mut iter)).await?;
            while iter.op_next(strand, Slot::reborrow(&mut item)).await? {
                match context {
                    SpreadContext::Args | SpreadContext::Sequence | SpreadContext::Pairs => {
                        sink.positional(strand, Slot::reborrow(&mut item))?
                    }
                }
            }
            Ok(())
        })
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
        match method.tag() {
            sym::GET_METHOD => {
                let ([field], []) = unpack!(strand, args, 1, 0)?;
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                Self::op_get(this, strand, field, out)
            }
            sym::SET_METHOD => {
                let ([field, value], []) = unpack!(strand, args, 2, 0)?;
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                Self::op_set(this, strand, field, value)
            }
            _ => {
                strand
                    .with_slots(async move |strand, [mut func]| {
                        Self::op_get(this, strand, method, Slot::reborrow(&mut func))?;
                        func.op_call(strand, args, out).await
                    })
                    .await
            }
        }
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

    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_display(this, strand, w)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        default_fmt::<Self>(this, strand, spec, w)
    }

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

    fn op_shl<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Err(Error::not_supported(strand))
    }

    fn op_shr<'a, 's>(
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
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::GET_METHOD | sym::SET_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::type_error(strand, "field get not supported")),
        }
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

pub(crate) fn default_fmt<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: Recv<'v, 'a, T>,
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    use crate::value::fmt::{Fill, Kind, Pad};

    let kind = spec
        .kind
        .ok_or_else(|| crate::value::fmt::unresolved_kind(strand))?;
    if !kind.is_text() {
        return Err(Error::type_error(
            strand,
            format!("unsupported format kind `:{}`", kind.symbol()),
        ));
    }
    if spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
        return Err(Error::type_error(strand, "unsupported format option"));
    }
    let mut pad = Pad::new(*spec, w);
    match kind {
        Kind::Str => T::op_display(this, strand, &mut pad)?,
        Kind::Dbg => T::op_debug(this, strand, &mut pad)?,
        Kind::Verbatim => T::op_verbatim(this, strand, &mut pad)?,
        _ => unreachable!(),
    }
    pad.finish(strand)
}

#[derive(Clone, Copy)]
enum BinOp {
    Eq,
    Ne,
    Band,
    Bor,
    Bxor,
    Shl,
    Shr,
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
    Verbatim,
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
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        // This introduces an implied 'v: 'a bound which can't be expressed explicitly
        _: &'a &'v (),
    ),
    op_subtype: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
        _: &'a &'v (),
    ) -> bool,
    op_inspect:
        for<'a> fn(this: ErasedRecv<'v, 'a>, vm: &Vm<'v>, _: &'a &'v ()) -> Option<Inspect<'v, 'a>>,
    op_fill: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_call: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_mcall: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_convert: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        op: FmtOp,
        w: &mut dyn Format<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_fmt: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_bool: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        _: &'a &'v (),
    ) -> bool,
    op_unary: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        op: UnaryOp,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_bin: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        op: BinOp,
        other: &Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_cmp: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        op: CmpOp,
        other: &Value<'v>,
        _: &'a &'v (),
    ) -> Result<'v, 's, Value<'v>>,
    op_get: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_set: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_index: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_assign: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_hash: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
        _: &'a &'v (),
    ) -> Result<'v, 's, ()>,
    op_next: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, bool>,
    op_put: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_iter: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_sink: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_spread: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
        _: &'a &'v (),
    ) -> Pinned<'v, 's, 'a, ()>,
    op_unpack: for<'a, 's> fn(
        this: ErasedRecv<'v, 'a>,
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
            op_convert: op_convert_glue::<T>,
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

impl<'v, T: Protocol<'v>> TypeHandle<'v, T> {
    pub(crate) fn create(&self, alloc: &mut impl Alloc<'v>, value: T, out: impl Output<'v>)
    where
        T::Annex: Default,
    {
        self.create_with_annex(alloc, value, Default::default(), out)
    }

    pub(crate) fn create_with_annex(
        &self,
        alloc: &mut impl Alloc<'v>,
        value: T,
        annex: T::Annex,
        mut out: impl Output<'v>,
    ) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        Slot::from_output(&mut out).store(Value::from_object(GcObj::new_annex(
            vm.arena(),
            *self,
            value,
            annex,
        )));
    }
}

impl<'v, T: ?Sized + Protocol<'v>> TypeHandle<'v, T> {
    /// Downcast `value` to this type, returning a [`RecvCast`] that must be entered via
    /// [`RecvCast::enter`]/[`RecvCast::enter_sync`] to obtain a [`Recv`]. This scopes the
    /// resulting borrow to the duration of a closure rather than handing back an
    /// indefinitely-long-lived one, mirroring [`Type::cast`](super::native::Type::cast) in
    /// the public extension API.
    // Only exercised by in-crate unit tests so far (see `object::dict::tests` and
    // `object::array_view::tests`); not yet adopted by any production call site, hence
    // the dead-code allowance outside test builds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn cast<'a>(&self, value: &'a Value<'v>) -> Option<RecvCast<'v, 'a, T>> {
        value.downcast_ref(*self).map(|borrow| RecvCast { borrow })
    }
}

/// Scoped downcast of a [`Value`] to a [`Recv`], obtained via [`TypeHandle::cast`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RecvCast<'v, 'a, T: ?Sized + Protocol<'v>> {
    borrow: gc::Borrow<'v, 'a, Header, T>,
}

impl<'v, 'a, T: ?Sized + Protocol<'v>> RecvCast<'v, 'a, T> {
    /// Obtain a [`Recv`] for the duration of `f`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn enter<'s, R>(
        self,
        strand: &mut Strand<'v, 's>,
        f: impl for<'x> AsyncFnOnce(&mut Strand<'v, 's>, Recv<'v, 'x, T>) -> R,
    ) -> R {
        f(strand, Recv::new(self.borrow)).await
    }

    /// Synchronous counterpart to [`RecvCast::enter`], for call sites that don't need to
    /// hold the `Recv` across an `.await`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn enter_sync<'s, R>(
        self,
        strand: &mut Strand<'v, 's>,
        f: impl for<'x> FnOnce(&mut Strand<'v, 's>, Recv<'v, 'x, T>) -> R,
    ) -> R {
        f(strand, Recv::new(self.borrow))
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
    delegator: Option<&'a Value<'v>>,
}

impl<'v, 'a, T: ?Sized + Boxable<Header>> Clone for Recv<'v, 'a, T> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver,
            delegator: self.delegator,
        }
    }
}

impl<'v, 'a, T: ?Sized + Boxable<Header>> Recv<'v, 'a, T> {
    pub(crate) fn new(receiver: gc::Borrow<'v, 'a, Header, T>) -> Self {
        Self {
            receiver,
            delegator: None,
        }
    }

    unsafe fn from_erased(receiver: ErasedRecv<'v, 'a>) -> Self {
        unsafe {
            Self {
                receiver: gc::Borrow::from_raw(receiver.header.cast()),
                delegator: receiver.delegator,
            }
        }
    }

    pub(crate) fn delegator(&self) -> Option<&'a Value<'v>> {
        self.delegator
    }

    pub(crate) fn with_delegator(mut self, delegator: &'a Value<'v>) -> Self {
        self.delegator = Some(delegator);
        self
    }

    pub(crate) fn as_header(&self) -> NonNull<Header> {
        self.receiver.as_header()
    }

    pub(crate) unsafe fn vtbl_downcast_unchecked<V: Upcast<arena::Vtbl>>(&self) -> &'a V {
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
        match self.delegator {
            Some(delegator) => InputBy::Borrow(delegator),
            None => InputBy::Value(Value::from_object(self.receiver.to_strong()), None),
        }
    }
}

impl<'v, 'a, T: ?Sized + Protocol<'v>> Input<'v> for &Recv<'v, 'a, T> {
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: Sealed) -> InputBy<'v, 'b> {
        match self.delegator {
            Some(delegator) => InputBy::Borrow(delegator),
            None => InputBy::Value(Value::from_object(self.receiver.to_strong()), None),
        }
    }
}

#[derive(Clone, Copy)]
struct ErasedRecv<'v, 'a> {
    header: NonNull<Header>,
    delegator: Option<&'a Value<'v>>,
}

fn op_type_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) {
    unsafe { T::op_type(Recv::from_erased(this), strand, out) }
}

fn op_subtype_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    supertype: &Value<'v>,
    _: &'a &'v (),
) -> bool {
    unsafe { T::op_subtype(Recv::from_erased(this), strand, supertype) }
}

fn op_inspect_glue<'v, 'a, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    vm: &Vm<'v>,
    _: &'a &'v (),
) -> Option<Inspect<'v, 'a>> {
    unsafe { T::op_inspect(Recv::from_erased(this), vm) }
}

fn op_fill_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    type_obj: &Value<'v>,
    native: Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_fill(Recv::from_erased(this), strand, type_obj, native) }
}

fn op_call_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_call(Recv::from_erased(this), strand, args, out).await
        })
    }
}

fn op_mcall_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_mcall(Recv::from_erased(this), strand, method, args, out).await
        })
    }
}

fn op_convert_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    op: FmtOp,
    w: &mut dyn Format<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe {
        let this = Recv::from_erased(this);
        match op {
            FmtOp::Verbatim => T::op_verbatim(this, strand, w),
            FmtOp::Display => T::op_display(this, strand, w),
            FmtOp::Debug => T::op_debug(this, strand, w),
        }
    }
}

fn op_fmt_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    spec: &Spec,
    w: &mut dyn Format<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_fmt(Recv::from_erased(this), strand, spec, w) }
}

fn to_bool_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    _: &'a &'v (),
) -> bool {
    unsafe { T::op_bool(Recv::from_erased(this), strand) }
}

fn op_unary_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    op: UnaryOp,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_erased(this);
        match op {
            UnaryOp::Neg => T::op_neg(this, strand),
            UnaryOp::Bnot => T::op_bnot(this, strand),
        }
    }
}

fn op_bin_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    op: BinOp,
    other: &'a Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_erased(this);
        match op {
            BinOp::Eq => T::op_eq(this, strand, other),
            BinOp::Ne => T::op_ne(this, strand, other),
            BinOp::Band => T::op_band(this, strand, other),
            BinOp::Bor => T::op_bor(this, strand, other),
            BinOp::Bxor => T::op_bxor(this, strand, other),
            BinOp::Shl => T::op_shl(this, strand, other),
            BinOp::Shr => T::op_shr(this, strand, other),
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
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    op: CmpOp,
    other: &'a Value<'v>,
    _: &'a &'v (),
) -> Result<'v, 's, Value<'v>> {
    unsafe {
        let this = Recv::from_erased(this);
        match op {
            CmpOp::Lt => T::op_lt(this, strand, other),
            CmpOp::Lte => T::op_lte(this, strand, other),
            CmpOp::Gt => T::op_gt(this, strand, other),
            CmpOp::Gte => T::op_gte(this, strand, other),
        }
    }
}

fn op_get_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_get(Recv::from_erased(this), strand, field, out) }
}

fn op_set_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    field: Sym<'v, 'a>,
    value: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_set(Recv::from_erased(this), strand, field, value) }
}

fn op_index_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    index: &Value<'v>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_index(Recv::from_erased(this), strand, index, out) }
}

fn op_assign_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    index: Slot<'v, 'a>,
    value: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_assign(Recv::from_erased(this), strand, index, value) }
}

fn op_hash_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    hasher: &mut DefaultHasher,
    _: &'a &'v (),
) -> Result<'v, 's, ()> {
    unsafe { T::op_hash(Recv::from_erased(this), strand, hasher) }
}

fn op_next_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, bool> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_next(Recv::from_erased(this), strand, out).await
        })
    }
}

fn op_put_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    item: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_put(Recv::from_erased(this), strand, item).await
        })
    }
}

fn op_iter_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_iter(Recv::from_erased(this), strand, out).await
        })
    }
}

fn op_sink_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_sink(Recv::from_erased(this), strand, out).await
        })
    }
}

fn op_spread_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    context: SpreadContext,
    sink: &'a mut dyn Spread<'v, 's>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_spread(Recv::from_erased(this), strand, context, sink).await
        })
    }
}

fn op_unpack_glue<'v, 'a, 's, T: ?Sized + Protocol<'v>>(
    this: ErasedRecv<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    sig: &'a Unpack<'v, 'a>,
    out: Slots<'v, 'a>,
    _: &'a &'v (),
) -> Pinned<'v, 's, 'a, ()> {
    unsafe {
        strand.pin_future_call(async move |strand| {
            T::op_unpack(Recv::from_erased(this), strand, sig, out).await
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
            let this = $obj.as_recv();
            let vtbl = this.header.as_ref().vtbl();
            (vtbl.$meth)(this, $strand, &&())
        }
    };
    ($obj: expr, $meth: ident, $strand: expr, $($params: expr),+) => {
        {
            let this = $obj.as_recv();
            let vtbl = this.header.as_ref().vtbl();
            (vtbl.$meth)(this, $strand, $($params),*, &&())
        }
    };
}

pub(crate) trait AsHeader {
    unsafe fn as_header(&self) -> NonNull<Header>;
}

trait AsRecv<'v, 'a> {
    unsafe fn as_recv(&self) -> ErasedRecv<'v, 'a>;
}

impl<'v, 'a, T: AsHeader> AsRecv<'v, 'a> for T {
    unsafe fn as_recv(&self) -> ErasedRecv<'v, 'a> {
        ErasedRecv {
            header: unsafe { self.as_header() },
            delegator: None,
        }
    }
}

pub(crate) struct Delegated<'v, 'a, T> {
    pub(crate) receiver: T,
    pub(crate) delegator: &'a Value<'v>,
}

impl<'v, 'a, T> Delegated<'v, 'a, T> {
    pub(crate) fn new(receiver: T, delegator: &'a Value<'v>) -> Self {
        Self {
            receiver,
            delegator,
        }
    }
}

impl<'v, 'a, T: AsHeader> AsRecv<'v, 'a> for Delegated<'v, 'a, T> {
    unsafe fn as_recv(&self) -> ErasedRecv<'v, 'a> {
        ErasedRecv {
            header: unsafe { self.receiver.as_header() },
            delegator: Some(self.delegator),
        }
    }
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

    fn op_fill<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_type<'s>(&self, strand: &'a mut Strand<'v, 's>, out: Slot<'v, 'a>);

    fn op_subtype<'s>(&self, strand: &'a mut Strand<'v, 's>, supertype: &Value<'v>) -> bool;

    fn op_inspect(&self, vm: &Vm<'v>) -> Option<Inspect<'v, 'a>>;

    fn op_verbatim<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_display<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_debug<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()>;

    fn op_fmt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
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

    fn op_shl<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>>;

    fn op_shr<'s>(
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

impl<'v, 'a, T: AsRecv<'v, 'a>> Dispatch<'v, 'a> for T {
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
        unsafe { invoke!(self, op_mcall, strand, method, args, out) }
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

    fn op_verbatim<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_convert, strand, FmtOp::Verbatim, w) }
    }

    fn op_display<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_convert, strand, FmtOp::Display, w) }
    }

    fn op_debug<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_convert, strand, FmtOp::Debug, w) }
    }

    fn op_fmt<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe { invoke!(self, op_fmt, strand, spec, w) }
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

    fn op_shl<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Shl, other) }
    }

    fn op_shr<'s>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        unsafe { invoke!(self, op_bin, strand, BinOp::Shr, other) }
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
pub(crate) async fn type_mcall_fallback<'v, 's>(
    strand: &mut Strand<'v, 's>,
    ty: &Value<'v>,
    method: Sym<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    // Handle the special case of a `(get)` or `(set)` intended for the class object itself
    // rather than qualified method invocation on an instance
    match method.tag() {
        sym::GET_METHOD if args.len() == 1 => {
            let ([field], []) = unpack!(strand, args, 1, 0)?;
            let field = field
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
            return ty.op_get(strand, field, out);
        }
        sym::SET_METHOD if args.len() == 2 => {
            let ([field, value], []) = unpack!(strand, args, 2, 0)?;
            let field = field
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
            return ty.op_set(strand, field, value);
        }
        _ => (),
    }

    let ([this], [], trailing) = unpack!(strand, args, 1, 0, ...)?;
    let (receiver, delegator) =
        if let Some(inst) = this.downcast_ref(strand.builtin_types().class_instance) {
            (
                get_native_slot(strand, inst, ty)
                    .ok_or_else(|| Error::type_error(strand, "not a native object subclass"))?,
                Some(&*this),
            )
        } else {
            strand.with_slots_sync(|strand, [mut tmp]| {
                this.op_type(strand, Slot::reborrow(&mut tmp));
                if !tmp.repr_eq(strand, ty) {
                    return Err(Error::type_error(strand, "invalid native object type"));
                }
                Ok((&*this, None))
            })?
        };

    if is_special_mcall(method.tag()) {
        special_mcall(strand, receiver, delegator, method, trailing, out).await
    } else {
        match delegator {
            Some(delegator) => {
                Delegated::new(receiver, delegator)
                    .op_mcall(strand, method, trailing, out)
                    .await
            }
            None => receiver.op_mcall(strand, method, trailing, out).await,
        }
    }
}

/// Handle the special methods supported by qualified native method calls.
/// All other symbols belong to the caller's ordinary method-call fallback.
pub(crate) async fn instance_mcall_fallback<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Option<Result<'v, 's, ()>> {
    if is_special_mcall(method.tag()) {
        let receiver = Value::from_input(strand.vm(), receiver);
        Some(special_mcall(strand, &receiver, None, method, args, out).await)
    } else {
        None
    }
}

pub(crate) fn is_special_mcall(tag: sym::Tag) -> bool {
    matches!(
        tag,
        sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::NEG_METHOD
            | sym::BNOT_METHOD
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
    )
}

async fn special_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    receiver: &Value<'v>,
    delegator: Option<&Value<'v>>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    macro_rules! dispatch {
        ($op:ident $(, $arg:expr)*) => {
            match delegator {
                Some(delegator) => Delegated::new(receiver, delegator).$op(strand $(, $arg)*),
                None => receiver.$op(strand $(, $arg)*),
            }
        };
    }

    match method.tag() {
        sym::STR_METHOD => {
            let mut format = crate::value::StrEmbryo::new();
            dispatch!(op_display, &mut format)?;
            format.finish(strand, out);
        }
        sym::DBG_METHOD => {
            let mut format = crate::value::StrEmbryo::new();
            dispatch!(op_debug, &mut format)?;
            format.finish(strand, out);
        }
        sym::FMT_METHOD => {
            let ([spec], []) = unpack!(strand, args, 1, 0)?;
            let spec = crate::stdlib::fmt::spec_of(strand, &spec)?;
            let mut format = crate::value::StrEmbryo::new();
            dispatch!(op_fmt, &spec, &mut format)?;
            format.finish(strand, out);
        }
        sym::BOOL_METHOD => {
            let b = dispatch!(op_bool);
            Output::set(strand, out, b);
        }
        sym::HASH_METHOD => {
            let mut hasher = DefaultHasher::new();
            dispatch!(op_hash, &mut hasher)?;
            Output::set(strand, out, hasher.finish());
        }
        sym::EQ_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            let value = match delegator {
                Some(delegator) => Delegated::new(receiver, delegator).op_eq(strand, &other)?,
                None => receiver.op_eq(strand, &other),
            };
            out.store(value);
        }
        sym::LT_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_lt, &other)?);
        }
        sym::NEG_METHOD => {
            out.store(dispatch!(op_neg)?);
        }
        sym::BNOT_METHOD => {
            out.store(dispatch!(op_bnot)?);
        }
        sym::ADD_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_add, &other)?);
        }
        sym::SUB_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_sub, &other)?);
        }
        sym::RSUB_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_rsub, &other)?);
        }
        sym::MUL_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_mul, &other)?);
        }
        sym::DIV_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_div, &other)?);
        }
        sym::RDIV_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_rdiv, &other)?);
        }
        sym::EDIV_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_ediv, &other)?);
        }
        sym::REDIV_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_rediv, &other)?);
        }
        sym::MOD_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_mod, &other)?);
        }
        sym::RMOD_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_rmod, &other)?);
        }
        sym::BAND_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_band, &other)?);
        }
        sym::BOR_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_bor, &other)?);
        }
        sym::BXOR_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_bxor, &other)?);
        }
        sym::SHL_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_shl, &other)?);
        }
        sym::SHR_METHOD => {
            let ([other], []) = unpack!(strand, args, 1, 0)?;
            out.store(dispatch!(op_shr, &other)?);
        }
        _ => unreachable!("special_mcall requires a supported symbol"),
    }
    Ok(())
}
