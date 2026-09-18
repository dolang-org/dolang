use std::{
    borrow::Cow,
    cell::UnsafeCell,
    collections::VecDeque,
    future,
    hash::Hasher,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{ControlFlow, Deref, DerefMut},
    ptr::NonNull,
};

use crate::value::fmt::{Format, Spec};

use bitvec::{bitbox, boxed::BitBox};

use crate::{
    arg::Args,
    error::{Error, ErrorKind, Result},
    gc::{
        self, Base, Collect,
        arena::{self, Upcast, Visit},
    },
    object::protocol::{Delegated, Dispatch, GcObjBorrow, Recv, Vtbl},
    sig,
    strand::{Pinned, Strand},
    sym::{self, Sym},
    unpack,
    value::{Case, Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::{Alloc, Register, Vm},
};

use super::{
    BoundMethod,
    field_iter::FieldIter,
    protocol::{
        self, Inspect, Member, MemberKind, Protocol, TypeHandle, instance_mcall_fallback,
        is_special_mcall, type_mcall_fallback,
    },
};
use dolang_bytecode::Variadic;
use dolang_util::alias;

pub use super::protocol::{Spread, SpreadContext};

pub(crate) struct ObjectWrap<'v, T> {
    value: ManuallyDrop<T>,
    finalized: bool,
    phantom: PhantomData<&'v mut &'v ()>,
}

impl<'v, T> Drop for ObjectWrap<'v, T> {
    fn drop(&mut self) {
        debug_assert!(self.finalized);
    }
}

pub(crate) struct ObjectAnnex<'v, T: Object<'v>> {
    slots: Option<alias::Box<[UnsafeCell<Value<'v>>]>>,
    inner: T::Annex,
}

impl<'v, T: Object<'v>> gc::Annex for ObjectAnnex<'v, T> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        if T::SLOTS != 0 {
            unsafe {
                for slot in self.slots.as_ref().unwrap_unchecked().iter() {
                    (*slot.get()).accept(visit)?
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn clear(&self) {
        // Zero out each slot using UnsafeCell interior mutability
        if T::SLOTS != 0 {
            unsafe {
                for slot in self.slots.as_ref().unwrap_unchecked().iter() {
                    *slot.get() = Value::NIL
                }
            }
        }
    }
}

/// Unpack operation specification and output slots.
///
/// This type is passed to [`Object::unpack`] implementations to describe what values
/// should be extracted from the object and where they should be stored. It supports:
///
/// - **Positional parameters**: Required and optional positional values
/// - **Keyed parameters**: Named values accessed by symbol or constant key
/// - **Variadic capture**: Capturing remaining values as an iterator
///
/// # Usage Pattern
///
/// Implementations should iterate over the unpack specification using [`iter()`](Self::iter),
/// which yields [`UnpackItem`] variants describing each parameter. For each item:
///
/// 1. Extract or compute the corresponding value from the object
/// 2. Store it in the provided [`Slot`]
/// 3. If the value is missing and required, return an error
/// 4. If the value is missing and optional, use the provided default
///
/// # Atomicity
///
/// Unpack operations should be **atomic** when practical: if unpacking fails partway through,
/// the object's observable state should remain unchanged.
pub struct Unpack<'v, 'a> {
    inner: &'a sig::Unpack<'v, 'a>,
    slots: Slots<'v, 'a>,
}

impl<'v, 'a> Unpack<'v, 'a> {
    /// Returns the number of required positional parameters.
    ///
    /// These must be provided or the unpack operation should fail.
    #[inline]
    pub fn required(&self) -> usize {
        self.inner.required
    }

    /// Returns the number of optional positional parameters.
    ///
    /// These have default values provided in the unpack specification.
    #[inline]
    pub fn optional(&self) -> usize {
        self.inner.optional.len()
    }

    /// Returns the number of required keyed parameters.
    ///
    /// These must be provided or the unpack operation should fail.
    #[inline]
    pub fn required_keys(&self) -> usize {
        self.inner
            .keys
            .iter()
            .map(|k| k.default.is_none() as usize)
            .sum()
    }

    /// Returns the first required key parameter, if present.
    #[inline]
    pub fn first_required_key(&self) -> Option<impl Input<'v> + '_> {
        self.inner.keys.iter().find_map(|k| {
            if k.default.is_none() {
                Some(&k.kind)
            } else {
                None
            }
        })
    }

    /// Returns the number of optional keyed parameters.
    ///
    /// These have default values provided in the unpack specification.
    #[inline]
    pub fn optional_keys(&self) -> usize {
        self.inner
            .keys
            .iter()
            .map(|k| k.default.is_some() as usize)
            .sum()
    }

    /// Returns if match must be exhaustive
    ///
    /// If true, the unpack operation should fail when the object contains additional
    /// unmatched items.  If false, iteration may yield a final [`U npackItem::Rest`]
    /// which should be populated with an iterator over any remaining items.
    #[inline]
    pub fn exhaustive(&self) -> bool {
        self.inner.variadic == Variadic::NONE
    }

    /// Returns if match has an [`UnpackItem::Rest`] element
    #[inline]
    pub fn rest(&self) -> bool {
        self.inner.variadic == Variadic::Capture
    }

    /// Returns an iterator over the unpack specification.
    ///
    /// The iterator yields [`UnpackItem`] variants in order:
    /// 1. Required positional items
    /// 2. Optional positional items
    /// 3. Key items (both required and optional)
    /// 4. Variadic rest (if applicable)
    pub fn iter(&mut self) -> UnpackIter<'v, 'a, '_> {
        UnpackIter { unpack: self, i: 0 }
    }
}

/// Iterator over unpack specification items.
///
/// Created by [`Unpack::iter()`]. Yields [`UnpackItem`] variants describing
/// each parameter that should be extracted from the object.
pub struct UnpackIter<'v, 'a, 'b> {
    unpack: &'b mut Unpack<'v, 'a>,
    i: usize,
}

/// A single parameter in an unpack operation.
///
/// Each variant describes what value to extract and where to store it:
///
/// - [`Pos`](Self::Pos): Positional parameter accessed by index
/// - [`SymKey`](Self::SymKey): Keyed parameter accessed by symbol
/// - [`ConstKey`](Self::ConstKey): Keyed parameter accessed by constant value
/// - [`Rest`](Self::Rest): Variadic capture of remaining values
///
/// All variants except `Rest` include an optional default value. If the value
/// cannot be extracted and a default is provided, store the default. If no
/// default is provided, the parameter is required and its absence should
/// result in an error.
pub enum UnpackItem<'v, 'a> {
    /// Positional parameter accessed by sequential index.
    ///
    /// Extract the value at the current position (tracked by implementation)
    /// and store it in `slot`. If the value is missing:
    /// - If `default` is `Some`, store the default value
    /// - If `default` is `None`, this is a required parameter and an error should be returned
    Pos {
        /// Output slot where the value should be stored.
        slot: Slot<'v, 'a>,
        /// Default value if the parameter is optional (None means required).
        default: Option<&'a Value<'v>>,
    },
    /// Keyed parameter accessed by symbol.
    ///
    /// Look up the value associated with `key` and store it in `slot`.
    /// The symbol should be compared by tag (identity), not by name.
    /// If the key is not found:
    /// - If `default` is `Some`, store the default value
    /// - If `default` is `None`, this is a required parameter and an error should be returned
    SymKey {
        /// Symbol key to look up.
        key: Sym<'v, 'a>,
        /// Output slot where the value should be stored.
        slot: Slot<'v, 'a>,
        /// Default value if the parameter is optional (None means required).
        default: Option<&'a Value<'v>>,
    },
    /// Keyed parameter accessed by constant value.
    ///
    /// Look up the value associated with `key` and store it in `slot`.
    /// The key should be compared by hash and equality, similar to dict/record lookup.
    /// If the key is not found:
    /// - If `default` is `Some`, store the default value
    /// - If `default` is `None`, this is a required parameter and an error should be returned
    ConstKey {
        /// Constant key to look up.
        key: &'a Value<'v>,
        /// Output slot where the value should be stored.
        slot: Slot<'v, 'a>,
        /// Default value if the parameter is optional (None means required).
        default: Option<&'a Value<'v>>,
    },
    /// Variadic capture of remaining values.
    ///
    /// Store an iterator (or other iterable value) in `slot` that will yield
    /// any values not consumed by earlier parameters. This is always the last
    /// item yielded by the iterator.
    ///
    /// For containers (dicts, records), this typically captures key-value pairs
    /// not matched by earlier keyed parameters. For sequential iterators, this
    /// captures remaining unconsumed values.
    Rest {
        /// Output slot where the iterator should be stored.
        slot: Slot<'v, 'a>,
    },
}

impl<'v, 'a, 'b> Iterator for UnpackIter<'v, 'a, 'b> {
    type Item = UnpackItem<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let unpack = &mut *self.unpack;
        let i = self.i;
        if i < unpack.inner.required {
            self.i += 1;
            Some(UnpackItem::Pos {
                slot: unsafe { unpack.slots.unchecked_at(i) },
                default: None,
            })
        } else if i < unpack.inner.required + unpack.inner.optional.len() {
            self.i += 1;
            Some(UnpackItem::Pos {
                slot: unsafe { unpack.slots.unchecked_at(i) },
                default: Some(&unpack.inner.optional[i - unpack.inner.required]),
            })
        } else if i < unpack.inner.required + unpack.inner.optional.len() + unpack.inner.keys.len()
        {
            self.i += 1;
            let key = &unpack.inner.keys[i - unpack.inner.required - unpack.inner.optional.len()];
            match &key.kind {
                sig::UnpackKeyKind::Sym(sym) => Some(UnpackItem::SymKey {
                    key: *sym,
                    slot: unsafe { unpack.slots.unchecked_at(i) },
                    default: key.default.as_ref(),
                }),
                sig::UnpackKeyKind::Const(value) => Some(UnpackItem::ConstKey {
                    key: value,
                    slot: unsafe { unpack.slots.unchecked_at(i) },
                    default: key.default.as_ref(),
                }),
            }
        } else if i == unpack.inner.required + unpack.inner.optional.len() + unpack.inner.keys.len()
            && unpack.inner.variadic == Variadic::Capture
        {
            self.i += 1;
            Some(UnpackItem::Rest {
                slot: unsafe { unpack.slots.unchecked_at(i) },
            })
        } else {
            None
        }
    }
}

/// Receiver for [`Object`] trait methods.
///
/// - Allows borrowing the underlying `T`
/// - Is an [`Input`] representing the object as a Do [`Value`].
pub struct Instance<'v, 'a, T: Object<'v>> {
    pub(crate) receiver: gc::Borrow<'v, 'a, protocol::Header, ObjectWrap<'v, T>>,
    /// When this operation arrived via class delegation, the original
    /// delegating value (e.g. the `ClassInstance`).  `None` for direct calls.
    pub(crate) delegator: Option<&'a Value<'v>>,
}

impl<'v, 'a, T: Object<'v>> Clone for Instance<'v, 'a, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, 'a, T: Object<'v>> Copy for Instance<'v, 'a, T> {}

/// Witness that a [`Value`] has dynamic type `T`, obtained from [`Type::downcast`].
///
/// Use [`Cast::enter`]/[`Cast::enter_sync`] to obtain an [`Instance`] for a
/// bounded scope.
#[must_use]
pub struct Cast<'v, 'a, T: Object<'v>> {
    receiver: gc::Borrow<'v, 'a, protocol::Header, ObjectWrap<'v, T>>,
    /// When the cast looked through a [`ClassInstance`](super::class::ClassInstance) native
    /// slot, the composite value it was found in.  `None` for a direct cast.
    delegator: Option<&'a Value<'v>>,
}

impl<'v, 'a, T: Object<'v>> Clone for Cast<'v, 'a, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, 'a, T: Object<'v>> Copy for Cast<'v, 'a, T> {}

impl<'v, 'a, T: Object<'v>> Cast<'v, 'a, T> {
    #[inline]
    fn enter_impl<'x>(&self) -> Instance<'v, 'x, T>
    where
        'a: 'x,
    {
        Instance {
            receiver: self.receiver,
            delegator: self.delegator,
        }
    }

    /// Obtain an [`Instance`] for the duration of `f`.
    #[inline]
    pub async fn enter<'s, R>(
        self,
        strand: &mut Strand<'v, 's>,
        f: impl for<'x> AsyncFnOnce(&mut Strand<'v, 's>, Instance<'v, 'x, T>) -> R,
    ) -> R {
        f(strand, self.enter_impl()).await
    }

    /// Synchronous counterpart to [`Cast::enter`], for call sites that don't need
    /// to hold the instance across an `.await`.
    #[inline]
    pub fn enter_sync<'s, R>(
        self,
        strand: &mut Strand<'v, 's>,
        f: impl for<'x> FnOnce(&mut Strand<'v, 's>, Instance<'v, 'x, T>) -> R,
    ) -> R {
        f(strand, self.enter_impl())
    }

    /// [`Cast::enter_sync`] without a `strand` argument, for the rare contexts —
    /// namely [`Object::finalize`] — that have no live [`Strand`] available.
    /// May be more expensive.
    #[inline]
    pub fn enter_finalize<R>(self, f: impl for<'x> FnOnce(Instance<'v, 'x, T>) -> R) -> R {
        f(self.enter_impl())
    }
}

/// Borrow of [`Object`] trait receiver.
pub struct Ref<'v, 'a, T: Object<'v>>(gc::Ref<'v, 'a, protocol::Header, ObjectWrap<'v, T>>);

impl<'v, 'a, T: Object<'v>> Ref<'v, 'a, T> {
    #[inline]
    pub fn slot<const N: usize>(this: &Self) -> &Value<'v> {
        const {
            assert!(N < T::SLOTS, "slot out of bounds");
        }
        unsafe {
            &*gc::Ref::annex(&this.0)
                .slots
                .as_ref()
                .unwrap_unchecked()
                .get_unchecked(N)
                .get()
        }
    }
}

impl<'v, 'a, T: Object<'v>> Deref for Ref<'v, 'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        debug_assert!(!self.0.finalized);
        &self.0.value
    }
}

impl<'v, 'a, T: Object<'v>> AsRef<T> for Ref<'v, 'a, T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self
    }
}

/// Mutable borrow of [`Object`] trait receiver.
pub struct Mut<'v, 'a, T: Object<'v>>(gc::Mut<'v, 'a, protocol::Header, ObjectWrap<'v, T>>);

impl<'v, 'a, T: Object<'v>> Mut<'v, 'a, T> {
    #[inline]
    pub fn slot<const N: usize>(this: &Self) -> &Value<'v> {
        const {
            assert!(N < T::SLOTS);
        }
        unsafe {
            &*gc::Mut::annex(&this.0)
                .slots
                .as_ref()
                .unwrap_unchecked()
                .get_unchecked(N)
                .get()
        }
    }

    #[inline]
    pub fn slot_mut<const N: usize>(this: &mut Self) -> Slot<'v, '_> {
        const {
            assert!(N < T::SLOTS, "slot out of bounds");
        }
        unsafe {
            Slot::new(
                &mut *gc::Mut::annex(&this.0)
                    .slots
                    .as_ref()
                    .unwrap_unchecked()
                    .get_unchecked(N)
                    .get(),
            )
        }
    }
}

impl<'v, 'a, T: Object<'v>> Deref for Mut<'v, 'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        debug_assert!(!self.0.finalized);
        &self.0.value
    }
}

impl<'v, 'a, T: Object<'v>> DerefMut for Mut<'v, 'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        debug_assert!(!self.0.finalized);
        &mut self.0.value
    }
}

impl<'v, 'a, T: Object<'v>> AsRef<T> for Mut<'v, 'a, T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self
    }
}

/// Annex guard.
///
/// Allows access to the immutable annex of an [`Object`] [`Instance`].
pub struct Annex<'v, 'a, T: Object<'v>>(&'a T::Annex);

impl<'v, 'a, T: Object<'v>> Deref for Annex<'v, 'a, T> {
    type Target = T::Annex;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'v, 'a, T: Object<'v>> AsRef<T::Annex> for Annex<'v, 'a, T> {
    fn as_ref(&self) -> &T::Annex {
        self
    }
}

impl<'v, 'a, T: Object<'v>> Input<'v> for Instance<'v, 'a, T> {
    #[allow(private_interfaces)]
    #[inline]
    fn input_take<'b>(&'b mut self, _vm: &'b Vm<'v>, _: Sealed) -> InputBy<'v, 'b> {
        match self.delegator {
            Some(delegator) => InputBy::Borrow(delegator),
            None => InputBy::Value(
                Value::from_object(Base::upcast(self.receiver.to_strong())),
                None,
            ),
        }
    }
}

impl<'v, 'a, T: Object<'v>> Instance<'v, 'a, T> {
    #[inline]
    fn borrow_impl<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        borrow: Option<gc::Ref<'v, 'a, protocol::Header, ObjectWrap<'v, T>>>,
    ) -> Result<'v, 's, Ref<'v, 'a, T>> {
        let borrow = borrow.ok_or_else(|| Error::concurrency(strand))?;
        if borrow.finalized {
            return Err(Error::runtime(strand, "native object finalized"));
        }
        Ok(Ref(borrow))
    }

    #[inline]
    fn borrow_mut_impl<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        borrow: Option<gc::Mut<'v, 'a, protocol::Header, ObjectWrap<'v, T>>>,
    ) -> Result<'v, 's, Mut<'v, 'a, T>> {
        let borrow = borrow.ok_or_else(|| Error::concurrency(strand))?;
        if borrow.finalized {
            return Err(Error::runtime(strand, "native object finalized"));
        }
        Ok(Mut(borrow))
    }

    #[inline]
    pub(crate) fn new(receiver: gc::Borrow<'v, 'a, protocol::Header, ObjectWrap<'v, T>>) -> Self {
        Self {
            receiver,
            delegator: None,
        }
    }

    #[inline]
    pub(crate) fn from_recv(this: &Recv<'v, 'a, ObjectWrap<'v, T>>) -> Self {
        Self {
            receiver: this.receiver,
            delegator: this.delegator(),
        }
    }

    pub(crate) fn from_native_value(value: &'a Value<'v>, vm: &Vm<'v>, ty: Type<'v, T>) -> Self {
        Self {
            receiver: value
                .downcast_native(vm, ty.vtbl)
                .expect("native view owner has wrong type"),
            delegator: Some(value),
        }
    }

    /// Borrow content immutably
    #[inline]
    pub fn borrow<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Ref<'v, 'a, T>> {
        self.borrow_impl(strand, self.receiver.borrow())
    }

    /// Borrow content immutably, panicking on failure
    #[inline]
    pub fn borrow_unwrap(&self) -> Ref<'v, 'a, T> {
        let borrow = self.receiver.borrow().expect("conflicting borrow");
        assert!(!borrow.finalized, "native object finalized");
        Ref(borrow)
    }

    /// Borrow content mutably
    #[inline]
    pub fn borrow_mut<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Mut<'v, 'a, T>> {
        self.borrow_mut_impl(strand, self.receiver.borrow_mut())
    }

    /// Borrow content mutably, panicking on failure
    #[inline]
    pub fn borrow_mut_unwrap(&self) -> Mut<'v, 'a, T> {
        let borrow = self.receiver.borrow_mut().expect("conflicting borrow");
        assert!(!borrow.finalized, "native object finalized");
        Mut(borrow)
    }

    /// Get immutable annex
    #[inline]
    pub fn annex(&self) -> Annex<'v, 'a, T> {
        Annex(&self.receiver.annex().inner)
    }

    /// Reconstructs the [`Type`] handle for this instance's own type.
    ///
    /// Lets a fixed [`Object`] trait method (one not registered as a
    /// `build()` closure, so it has no other way to capture a `Type` handle
    /// — e.g. [`Object::eq`], [`Object::band`]) obtain its own type in order
    /// to [`Type::cast`] another value into the same type, or construct a
    /// new instance of it.
    pub fn ty(&self, vm: &Vm<'v>) -> Type<'v, T> {
        // SAFETY: `self.receiver` is a live `ObjectWrap<'v,T>` receiver.
        let vtbl = unsafe {
            self.receiver
                .as_header()
                .as_ref()
                .vtbl_downcast_unchecked::<ObjectVtbl<'v>>()
        };
        let Case::Object(singleton) = vm.type_singletons[vtbl.singleton_idx].case() else {
            unreachable!("type singleton is always an object");
        };
        // SAFETY: `type_singletons[idx]` is always a `TypeObjectWrap<'v,T>` for this `T`.
        unsafe { Type::<T>::from_type_header(singleton.into_raw().cast(), vm) }
    }
}

/// Native object.
///
/// Implementing this trait allows values of the given type to
/// be turned into Do values with [`Type::create`].
pub trait Object<'v>: Sized + 'v {
    /// Name of owning Do module.
    const MODULE: &'v str;
    /// Name of type.
    const NAME: &'v str;
    /// Number of [`Value`] slots available.  Access these with [`Ref::slot`] and [`Mut::slot`].
    const SLOTS: usize = 0;
    /// Immutable annex.  An object's annex can be accessed via [`Instance::annex`] without a
    /// runtime borrow check.
    type Annex: 'v;

    /// Main mutable struct for the type object singleton (borrow-checked at runtime).
    /// Accessible via [`Type::borrow`] / [`Type::borrow_mut`] guards.
    /// Use `()` for types with no mutable type state.
    type Type: 'v;

    /// Immutable annex for the type object singleton (no runtime borrow check).
    /// Accessible via [`Type::annex`].
    /// Use `()` for types with no immutable type data.
    type TypeAnnex: 'v;

    /// Called when the type object singleton is invoked as a function.
    ///
    /// # Default
    ///
    /// Raise an error due to the type not being explicitly instantiable.
    #[allow(unused_variables)]
    fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(strand, "not instantiable")))
    }

    /// Implements verbatim string conversion by writing a string representation to `w`.
    fn verbatim<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::display(this, strand, w)
    }

    /// Implements canonical string conversion by writing a string representation to `w`.
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::debug(this, strand, w)
    }

    /// Implements debug string conversion by writing a string representation to `w`.
    /// Compared to [`Object::display`], which is intended to be canonical and user-readable,
    /// debug string forms are intended for developer consumption.
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let _ = this;
        crate::fmt!(strand, w, "<{}.{}>", Self::MODULE, Self::NAME)
    }

    /// Implements formatting according to a resolved specification.
    fn fmt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        use crate::value::fmt::{Fill, Kind, Pad};

        let kind = spec
            .kind
            .ok_or_else(|| crate::value::fmt::unresolved_kind(strand))?;
        if !kind.is_text() || spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
            return Err(Error::type_error(strand, "unsupported format option"));
        }
        let mut pad = Pad::new(*spec, w);
        match kind {
            Kind::Str => Self::display(this, strand, &mut pad)?,
            Kind::Dbg => Self::debug(this, strand, &mut pad)?,
            Kind::Verbatim => Self::verbatim(this, strand, &mut pad)?,
            _ => unreachable!(),
        }
        pad.finish(strand)
    }

    /// Implements boolean conversion.
    ///
    /// # Default
    /// Native objects are truthy.
    #[allow(unused_variables)]
    fn bool<'a, 's>(this: Instance<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        true
    }

    /// Implements Do call (`func arg...`) operations.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("call not supported: {}", Self::NAME),
        )))
    }

    /// Register methods and fields during type registration.
    ///
    /// Called to populate the per-type method dispatch table.
    ///
    /// # Default
    /// Register no methods
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
    }

    /// Implements dynamic method lookup on the type object.
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn type_method<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::field(strand, method)))
    }

    /// Implements dynamic field lookup on the type object.
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn type_get<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::field(strand, field))
    }

    /// Implements dynamic field assignment on the type object.
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn type_set<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::field(strand, field))
    }

    /// Implements Do indexing (`Type[index]`) operations on the type object.
    ///
    /// # Default
    /// Returns a type error.
    #[allow(unused_variables)]
    fn type_index<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("indexing not supported: {}", Self::NAME),
        ))
    }

    /// Implements Do method call (`func.meth arg...`) operations
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn method<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::field(strand, method)))
    }

    /// Implements Do get (`obj.field`) operations
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn get<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::field(strand, field))
    }

    /// Implements Do set (`obj.field = value`) operations
    /// # Default
    /// Returns a field error
    #[allow(unused_variables)]
    fn set<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::field(strand, field))
    }

    /// Implements Do indexing (`obj[index]`) operations
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("indexing not supported: {}", Self::NAME),
        ))
    }

    /// Implements Do index assignment (`obj[index] = value`) operations
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn assign<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("index assignment not supported: {}", Self::NAME),
        ))
    }

    /// Implements Do iteration (`for value = iteratee`), setting `out` to an
    /// iterator object which should implement [`Object::next`]
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("iteration not supported: {}", Self::NAME),
        )))
    }

    /// Implements Do iteration (`for value = iteratee`), setting `out` to the
    /// next value (if applicable) and returning `true` if a next value was available,
    /// or `false` if iteration is concluded.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,

        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, bool>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("iterator `next` not supported: {}", Self::NAME),
        )))
    }

    /// Implements Do sink outputsetting `out` to a
    /// sink object which should implement [`Object::put`]
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("sink coercion not supported: {}", Self::NAME),
        )))
    }

    /// Implements Do output iteration (`send`), inserting `value` into the output.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("sink `put` not supported: {}", Self::NAME),
        )))
    }

    /// Implements spread in a context-sensitive way.
    ///
    /// # Default
    ///
    /// Returns [`Error::not_supported`], which causes the runtime to fall back to the
    /// generic protocol adapter based on iteration.
    #[allow(unused_variables)]
    fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::not_supported(strand)))
    }

    /// Computes a hash for this object.
    /// # Default
    /// Hashes the object's memory address.
    #[allow(unused_variables)]
    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        use std::ptr;
        ptr::hash(this.receiver.into_raw().as_ptr(), hasher);
        Ok(())
    }

    /// Compares this object to another for equality.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Err(Error::type_error(
            strand,
            format!("equality not supported: {}", Self::NAME),
        ))
    }

    /// Compares this object to another for inequality.
    /// # Default
    /// Delegates to [`eq`](Self::eq) and negates.
    fn ne<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Self::eq(this, strand, other).map(|b| !b)
    }

    /// Computes the boolean negation of this object.
    /// # Default
    /// Returns false.
    #[allow(unused_variables)]
    fn not<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(false)
    }

    /// Computes the arithmetic negation of this object.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn neg<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("negation not supported: {}", Self::NAME),
        ))
    }

    /// Computes the bitwise complement of this object.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn bnot<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("bitwise inverse not supported: {}", Self::NAME),
        ))
    }

    /// Computes the sum of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn add<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("addition not supported: {}", Self::NAME),
        ))
    }

    /// Computes the difference of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn sub<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("subtraction not supported: {}", Self::NAME),
        ))
    }

    /// Computes the difference of another and this object (reverse subtraction).
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn rsub<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("subtraction not supported: {}", Self::NAME),
        ))
    }

    /// Computes the product of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn mul<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("multiplication not supported: {}", Self::NAME),
        ))
    }

    /// Computes the quotient of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn div<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("division not supported: {}", Self::NAME),
        ))
    }

    /// Computes the quotient of another and this object (reverse division).
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn rdiv<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("division not supported: {}", Self::NAME),
        ))
    }

    /// Computes the Euclidean quotient of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn ediv<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("Euclidean division not supported: {}", Self::NAME),
        ))
    }

    /// Computes the Euclidean quotient of another and this object.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn redv<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("Euclidean division not supported: {}", Self::NAME),
        ))
    }

    /// Computes the remainder of this object divided by another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn rem<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("remainder not supported: {}", Self::NAME),
        ))
    }

    /// Computes the remainder of another divided by this object.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn rrem<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("remainder not supported: {}", Self::NAME),
        ))
    }

    /// Computes the bitwise AND of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn band<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("bitwise and not supported: {}", Self::NAME),
        ))
    }

    /// Computes the bitwise OR of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn bor<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("bitwise or not supported: {}", Self::NAME),
        ))
    }

    /// Computes the bitwise XOR of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn bxor<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("bitwise xor not supported: {}", Self::NAME),
        ))
    }

    /// Computes the left shift of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn shl<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("left shift not supported: {}", Self::NAME),
        ))
    }

    /// Computes the right shift of this object and another.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn shr<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            format!("right shift not supported: {}", Self::NAME),
        ))
    }

    /// Compares this object to another for less-than ordering.
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Err(Error::type_error(
            strand,
            format!("comparison not supported: {}", Self::NAME),
        ))
    }

    /// Compares this object to another for less-than-or-equal ordering.
    /// # Default
    /// Computes from [`lt`](Self::lt) and [`eq`](Self::eq).
    fn lte<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Ok(Self::lt(this, strand, other)? || Self::eq(this, strand, other)?)
    }

    /// Compares this object to another for greater-than ordering.
    /// # Default
    /// Computes from [`lte`](Self::lte).
    fn gt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Ok(!Self::lte(this, strand, other)?)
    }

    /// Compares this object to another for greater-than-or-equal ordering.
    /// # Default
    /// Computes from [`lt`](Self::lt).
    fn gte<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        Ok(!Self::lt(this, strand, other)?)
    }

    /// Unpacks values from this object according to the provided specification.
    ///
    /// This method is called when the object appears on the right side of a destructuring
    /// assignment, such as:
    /// ```dolang
    /// let a b c = some_obj
    /// let :x :y :z = some_obj
    /// ```
    ///
    /// The [`Unpack`] parameter describes what values to extract and where to store them.
    /// Implementations should iterate over the specification and populate the output slots
    /// with values from the object.
    ///
    /// # Default Implementation
    ///
    /// Unpacks registered readable fields.
    ///
    /// # Atomicity
    ///
    /// When practical, implementations should make unpacking **atomic**: if the operation
    /// fails partway through, the object's observable state should remain unchanged.
    /// See [`Unpack`] documentation for patterns and examples.
    fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        default_object_unpack(this, strand, unpack)
    }

    /// Performs any object-specific finalization before the main object state is dropped.
    ///
    /// This method is called synchronously just before the runtime drops the main object state.
    /// Unlike [`Drop::drop`], this method allows access to the object's annex and slots.
    ///
    /// `finalize` runs with no outstanding runtime borrows of the object, and
    /// [`Instance::borrow_mut_unwrap`] is expected to succeed for `this` during the call.
    ///
    /// # Default
    /// Does nothing.
    #[allow(unused_variables)]
    fn finalize<'a>(this: Instance<'v, 'a, Self>) {}
}

fn readable_entry(entry: &Entry<'_>) -> bool {
    matches!(
        entry,
        Entry::Getter(_)
            | Entry::Property(_, _)
            | Entry::Delegate(_, MemberKind::Getter | MemberKind::Property)
    )
}

async fn default_object_unpack<'v, 'a, 's, T: Object<'v>>(
    this: Instance<'v, 'a, T>,
    strand: &'a mut Strand<'v, 's>,
    unpack: Unpack<'v, 'a>,
) -> Result<'v, 's, ()> {
    let sig = unpack.inner;
    let mut out = unpack.slots;
    let pos_count = sig.required + sig.optional.len();
    if sig.required != 0 {
        return Err(Error::missing_positional(strand, 0));
    }

    strand
        .with_slots_dynamic(sig.len(), async |strand, mut staged| {
            for (index, default) in sig.optional.iter().enumerate() {
                staged.at(index).store(default.dup());
            }

            let recv = match this.delegator {
                Some(delegator) => {
                    Recv::<ObjectWrap<'v, T>>::new(this.receiver).with_delegator(delegator)
                }
                None => Recv::<ObjectWrap<'v, T>>::new(this.receiver),
            };
            let entries = &recv.vtbl().entries;
            let track = sig.variadic != Variadic::Discard;
            let mut matched: Option<BitBox> = track.then(|| bitbox![0; entries.len()]);

            for (key_index, key) in sig.keys.iter().enumerate() {
                let dest = pos_count + key_index;
                let found = match &key.kind {
                    sig::UnpackKeyKind::Sym(sym) => entries
                        .binary_search_by_key(sym, |(candidate, _)| *candidate)
                        .ok()
                        .filter(|index| readable_entry(&entries[*index].1)),
                    sig::UnpackKeyKind::Const(_) => None,
                };
                let present = if let Some(index) = found {
                    let (sym, entry) = &entries[index];
                    let result = match entry {
                        Entry::Getter(handler) | Entry::Property(handler, _) => unsafe {
                            handler.call(
                                recv.as_header(),
                                recv.delegator(),
                                strand,
                                staged.at(dest),
                            )
                        },
                        Entry::Delegate(supertype_index, _) => {
                            let supertype =
                                instance_supertype::<T>(recv.clone(), strand, *supertype_index);
                            strand.with_slots_sync(|strand, [mut delegator]| {
                                Output::set(strand, Slot::reborrow(&mut delegator), &recv);
                                Delegated::new(supertype, &delegator).op_get(
                                    strand,
                                    *sym,
                                    staged.at(dest),
                                )
                            })
                        }
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(()) => true,
                        Err(error) if error.kind() == ErrorKind::Field => false,
                        Err(error) => return Err(error),
                    }
                } else {
                    false
                };
                if !present {
                    if let Some(default) = &key.default {
                        staged.at(dest).store(default.dup());
                    } else {
                        return Err(match &key.kind {
                            sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                            sig::UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                        });
                    }
                }
                if let (Some(index), Some(matched)) = (found, &mut matched) {
                    matched.set(index, true);
                }
            }

            if sig.variadic == Variadic::NONE {
                let matched = matched.as_mut().unwrap();
                for (index, (sym, entry)) in entries.iter().enumerate() {
                    if matched[index] || !readable_entry(entry) {
                        continue;
                    }
                    let present = strand.with_slots_sync(|strand, [tmp]| {
                        let result = match entry {
                            Entry::Getter(handler) | Entry::Property(handler, _) => unsafe {
                                handler.call(recv.as_header(), recv.delegator(), strand, tmp)
                            },
                            Entry::Delegate(supertype_index, _) => {
                                let supertype =
                                    instance_supertype::<T>(recv.clone(), strand, *supertype_index);
                                strand.with_slots_sync(|strand, [mut delegator]| {
                                    Output::set(strand, Slot::reborrow(&mut delegator), &recv);
                                    Delegated::new(supertype, &delegator).op_get(strand, *sym, tmp)
                                })
                            }
                            _ => unreachable!(),
                        };
                        match result {
                            Ok(()) => Ok(true),
                            Err(error) if error.kind() == ErrorKind::Field => Ok(false),
                            Err(error) => Err(error),
                        }
                    })?;
                    if present {
                        return Err(Error::unexpected_key(strand, *sym));
                    }
                    matched.set(index, true);
                }
            }

            if sig.variadic == Variadic::Capture {
                let matched = matched.as_ref().unwrap();
                let symbols = entries
                    .iter()
                    .enumerate()
                    .filter(|(index, (_, entry))| !matched[*index] && readable_entry(entry))
                    .map(|(_, (sym, _))| strand.sym_obj(*sym))
                    .collect::<VecDeque<_>>();
                let receiver = Value::from_object(Base::upcast(this.receiver.to_strong()));
                strand.builtin_types().field_iter.create(
                    strand,
                    FieldIter::new(receiver, symbols),
                    staged.at(sig.len() - 1),
                );
            }

            for index in 0..sig.len() {
                out.at(index).store(staged.at(index).take());
            }
            Ok(())
        })
        .await
}

unsafe impl<'v, T: Object<'v>> Collect for ObjectWrap<'v, T> {
    const CYCLIC: bool = T::SLOTS != 0;
    const IMMUTABLE: bool = false;
    type Annex = ObjectAnnex<'v, T>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn finalize(this: NonNull<arena::Header>) {
        unsafe {
            let obj: protocol::GcObjBorrow<Self> = gc::Borrow::new(this.cast());
            if obj
                .borrow()
                .expect("object borrowed during finalize")
                .finalized
            {
                return;
            }
            T::finalize(Instance::new(obj));
            let mut borrow = obj
                .borrow_mut()
                .expect("borrow guard leaked during finalize");
            ManuallyDrop::drop(&mut borrow.value);
            borrow.finalized = true;
        }
    }

    fn clear(&mut self) {}
}

impl<'v, 'a, T: Object<'v>> Recv<'v, 'a, ObjectWrap<'v, T>> {
    fn vtbl(&self) -> &ObjectVtbl<'v> {
        unsafe { self.vtbl_downcast_unchecked::<ObjectVtbl<'v>>() }
    }

    fn entry(&self, sym: Sym<'v, '_>) -> Option<&Entry<'v>> {
        self.vtbl().entry(sym)
    }

    /// Returns a reference to the type singleton [`Value`] for this type.
    fn singleton<'b>(&self, vm: &'b Vm<'v>) -> &'b Value<'v> {
        &vm.type_singletons[self.vtbl().singleton_idx]
    }
}

impl<'v, T: Object<'v>> Protocol<'v> for ObjectWrap<'v, T> {
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(fmt)")),
            |strand| T::fmt(Instance::from_recv(&this), strand, spec, w),
        )
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(str)")),
            |strand| T::display(Instance::from_recv(&this), strand, w),
        )
    }

    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(verbatim)")),
            |strand| T::verbatim(Instance::from_recv(&this), strand, w),
        )
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(dbg)")),
            |strand| T::debug(Instance::from_recv(&this), strand, w),
        )
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        T::bool(Instance::from_recv(&this), strand)
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            None,
            async |strand| T::call(Instance::from_recv(&this), strand, args, out).await,
        )
        .await
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some((sym, entry)) = this.vtbl().entry_with_sym(method) {
            let name = sym.as_str(strand);
            Strand::async_for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed(name)),
                async |strand| match entry {
                    Entry::Method(handler) => unsafe {
                        handler
                            .call(this.as_header(), this.delegator(), strand, args, out)
                            .await
                    },
                    Entry::Delegate(idx, MemberKind::Method) => {
                        let supertype = instance_supertype::<T>(this.clone(), strand, *idx);
                        strand
                            .with_slots(async |strand, [mut delegator]| {
                                Output::set(strand, Slot::reborrow(&mut delegator), &this);
                                Delegated::new(supertype, &delegator)
                                    .op_mcall(strand, method, args, out)
                                    .await
                            })
                            .await
                    }
                    _ => Err(Error::field(strand, method)),
                },
            )
            .await
        } else {
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
                _ if is_special_mcall(method.tag()) => {
                    instance_mcall_fallback(strand, &this, method, args, out)
                        .await
                        .expect("supported special method")
                }
                _ => T::method(Instance::from_recv(&this), strand, method, args, out).await,
            }
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(get)")),
            |strand| match this.entry(field) {
                Some(Entry::Getter(handler) | Entry::Property(handler, _)) => unsafe {
                    handler.call(this.as_header(), this.delegator(), strand, out)
                },
                Some(Entry::Method(_) | Entry::Delegate(_, MemberKind::Method)) => {
                    BoundMethod::create(strand, &this, field, out);
                    Ok(())
                }
                Some(Entry::Setter(_) | Entry::Delegate(_, _)) => Err(Error::field(strand, field)),
                None => T::get(Instance::from_recv(&this), strand, field, out),
            },
        )
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(set)")),
            |strand| match this.entry(field) {
                Some(Entry::Setter(handler) | Entry::Property(_, handler)) => unsafe {
                    handler.call(this.as_header(), this.delegator(), strand, value)
                },
                Some(Entry::Getter(_)) => Err(Error::immutable(strand)),
                _ => T::set(Instance::from_recv(&this), strand, field, value),
            },
        )
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(index)")),
            |strand| T::index(Instance::from_recv(&this), strand, index, out),
        )
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(assign)")),
            |strand| T::assign(Instance::from_recv(&this), strand, index, value),
        )
    }

    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        let input = this.singleton(strand.vm());
        Output::set(strand, out, input);
    }

    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        _supertype: &Value<'v>,
    ) -> bool {
        // Instance objects are not type objects; subtype checks are handled by type objects.
        false
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(iter)")),
            async |strand| T::iter(Instance::from_recv(&this), strand, out).await,
        )
        .await
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(next)")),
            async |strand| T::next(Instance::from_recv(&this), strand, out).await,
        )
        .await
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(sink)")),
            async |strand| T::sink(Instance::from_recv(&this), strand, out).await,
        )
        .await
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(put)")),
            async |strand| T::put(Instance::from_recv(&this), strand, value).await,
        )
        .await
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        T::hash(Instance::from_recv(&this), _strand, hasher)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(eq)")),
            |strand| T::eq(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    fn op_ne<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(ne)")),
            |strand| T::ne(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    fn op_neg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(neg)")),
            |strand| T::neg(Instance::from_recv(&this), strand, Slot::new(&mut out)),
        )?;
        Ok(out)
    }

    fn op_bnot<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(bnot)")),
            |strand| T::bnot(Instance::from_recv(&this), strand, Slot::new(&mut out)),
        )?;
        Ok(out)
    }

    fn op_band<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(band)")),
            |strand| {
                T::band(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_bor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(bor)")),
            |strand| {
                T::bor(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_bxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(bxor)")),
            |strand| {
                T::bxor(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_shl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(shl)")),
            |strand| {
                T::shl(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_shr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(shr)")),
            |strand| {
                T::shr(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_add<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(add)")),
            |strand| {
                T::add(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_sub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(sub)")),
            |strand| {
                T::sub(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_rsub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(rsub)")),
            |strand| {
                T::rsub(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_mul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(mul)")),
            |strand| {
                T::mul(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_div<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(div)")),
            |strand| {
                T::div(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_rdiv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(rdiv)")),
            |strand| {
                T::rdiv(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_ediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(ediv)")),
            |strand| {
                T::ediv(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_rediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(rediv)")),
            |strand| {
                T::redv(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_mod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(mod)")),
            |strand| {
                T::rem(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_rmod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let mut out = Value::NIL;
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(rmod)")),
            |strand| {
                T::rrem(
                    Instance::from_recv(&this),
                    strand,
                    other,
                    Slot::new(&mut out),
                )
            },
        )?;
        Ok(out)
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(lt)")),
            |strand| T::lt(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    fn op_lte<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(lte)")),
            |strand| T::lte(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    fn op_gt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(gt)")),
            |strand| T::gt(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    fn op_gte<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let result = Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(gte)")),
            |strand| T::gte(Instance::from_recv(&this), strand, other),
        )?;
        Ok(Value::from_bool(result))
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(unpack)")),
            async |strand| {
                T::unpack(
                    Instance::from_recv(&this),
                    strand,
                    Unpack {
                        inner: sig,
                        slots: out,
                    },
                )
                .await
            },
        )
        .await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: protocol::SpreadContext,
        sink: &'a mut dyn protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        match Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(spread)")),
            async |strand| T::spread(Instance::from_recv(&this), strand, context, sink).await,
        )
        .await
        {
            Err(err) if err.kind() == crate::error::ErrorKind::Unsupported => {
                protocol::default_spread(strand, this.clone(), context, sink).await
            }
            other => other,
        }
    }
}

/// Registered object type.
///
/// Allows instantiating native objects and downcasting [`Value`]s to obtain immutable or
/// mutable reference guards to objects of that type.
///
/// Implements [`Input`] — pass directly to
/// [`ModuleBuilder::value`](crate::vm::ModuleBuilder::value) to expose the
/// type object singleton as a module item.
pub struct Type<'v, T: Object<'v>> {
    pub(crate) vtbl: TypeHandle<'v, ObjectWrap<'v, T>>,
    /// Vtable for the type object singleton
    pub(crate) type_vtbl: TypeHandle<'v, TypeObjectWrap<'v, T>>,
    /// Index into `Vm::type_singletons` for `Input` and `op_type` dispatch.
    pub(crate) singleton_idx: usize,
    /// Handle for [`ClassInstance`](super::class::ClassInstance), so that [`Type::cast`] can
    /// look through the native slots of a Do subclass.
    ///
    /// A handle rather than a `&Vm` deliberately: it is a plain vtable pointer, so it needs
    /// no VM borrow to carry around (which would be unsound to mint from `&mut Builder` at
    /// registration time) and stays usable where no [`Vm`] reference exists at all, such as
    /// [`Object::finalize`].
    pub(crate) class_instance: TypeHandle<'v, super::class::ClassInstance<'v>>,
}

impl<'v, T: Object<'v>> Copy for Type<'v, T> {}

impl<'v, T: Object<'v>> Clone for Type<'v, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, T: Object<'v>> Input<'v> for Type<'v, T> {
    #[allow(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: Sealed) -> InputBy<'v, 'b> {
        InputBy::Borrow(self.singleton(vm))
    }
}

impl<'v, T: Object<'v>> Type<'v, T> {
    /// Returns the type object singleton value.
    ///
    /// Unlike [`Input::input_take`], the returned reference borrows only from `vm`, so this
    /// may be called on a temporary handle (e.g. one just fetched from VM state).
    pub(crate) fn singleton<'b>(&self, vm: &'b Vm<'v>) -> &'b Value<'v> {
        &vm.type_singletons[self.singleton_idx]
    }

    pub(crate) fn create_raw(&self, vm: &Vm<'v>, value: T, annex: T::Annex) -> Value<'v> {
        Value::from_object(protocol::GcObj::new_annex(
            vm.arena(),
            self.vtbl,
            ObjectWrap {
                value: ManuallyDrop::new(value),
                finalized: false,
                phantom: PhantomData,
            },
            ObjectAnnex {
                slots: if T::SLOTS != 0 {
                    Some(
                        (0..T::SLOTS)
                            .map(|_| UnsafeCell::new(Value::NIL))
                            .collect::<Vec<_>>()
                            .into(),
                    )
                } else {
                    None
                },
                inner: annex,
            },
        ))
    }

    /// Instantiates a native object and places it into the provided output
    pub fn create(&self, alloc: &mut impl Alloc<'v>, value: T, mut out: impl Output<'v>)
    where
        T::Annex: Default,
    {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        Slot::from_output(&mut out).store(self.create_raw(vm, value, Default::default()))
    }

    /// Instantiates a native object with an annex and places it into the provided output
    pub fn create_with_annex(
        &self,
        alloc: &mut impl Alloc<'v>,
        value: T,
        annex: T::Annex,
        mut out: impl Output<'v>,
    ) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        Slot::from_output(&mut out).store(self.create_raw(vm, value, annex))
    }

    /// Check whether `value` has dynamic type `T`, returning a witness suitable to obtain an
    /// [`Instance`].
    ///
    /// A Do class deriving from `T` is a
    /// [`ClassInstance`](super::class::ClassInstance) holding the native object in a slot,
    /// so a direct vtable match is tried first and its native slots second — matching the
    /// Do-side `type value T` check.  When the native is found that way, the composite value
    /// is retained as the resulting [`Instance`]'s delegator, so the instance still refers to
    /// the whole object rather than the bare delegate.
    pub fn cast<'a>(&self, value: &'a Value<'v>) -> Option<Cast<'v, 'a, T>> {
        if let Some(receiver) = value.downcast_ref(self.vtbl) {
            return Some(Cast {
                receiver,
                delegator: None,
            });
        }
        value
            .downcast_native_with(self.class_instance, self.vtbl)
            .map(|receiver| Cast {
                receiver,
                delegator: Some(value),
            })
    }

    /// Reconstructs a `Type<'v,T>` from a raw type object GC header.
    ///
    /// # Safety
    /// `header` must point to a GC-allocated `TypeObjectWrap<'v,T>` whose vtable is
    /// `TypeVtbl<'v>`.
    pub(crate) unsafe fn from_type_header(header: NonNull<arena::Header>, vm: &Vm<'v>) -> Self {
        let vtbl = unsafe { header.as_ref().vtbl().cast::<TypeVtbl<'v>>() };
        Type {
            vtbl: unsafe { TypeHandle::new(vtbl.as_ref().inst_vtbl.cast()) },
            type_vtbl: unsafe { TypeHandle::new(vtbl.cast()) },
            singleton_idx: unsafe { vtbl.as_ref().inst_vtbl.as_ref().singleton_idx },
            class_instance: vm.builtin_types().class_instance,
        }
    }

    /// Returns a `gc::Borrow<'v,'v,...>` to the type singleton.
    fn type_borrow_impl(
        self,
        vm: &Vm<'v>,
    ) -> gc::Borrow<'v, 'v, protocol::Header, TypeObjectWrap<'v, T>> {
        // SAFETY: type_singletons[singleton_idx] is always a TypeObjectWrap<T>.
        let borrow = unsafe {
            vm.type_singletons[self.singleton_idx]
                .downcast_ref(self.type_vtbl)
                .unwrap_unchecked()
        };
        // SAFETY: the singleton lives for 'v; transmuting the borrow lifetime to 'v is sound.
        unsafe { std::mem::transmute(borrow) }
    }

    /// Runtime shared borrow of `T::Type`.
    pub fn borrow<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, TypeRef<'v, T>> {
        Ok(TypeRef(
            self.type_borrow_impl(strand)
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?,
        ))
    }

    /// Runtime shared borrow of `T::Type`, panicking on failure.
    pub fn borrow_unwrap(&self, vm: &Vm<'v>) -> TypeRef<'v, T> {
        TypeRef(
            self.type_borrow_impl(vm)
                .borrow()
                .expect("conflicting borrow"),
        )
    }

    /// Runtime exclusive borrow of `T::Type`.
    pub fn borrow_mut<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, TypeMut<'v, T>> {
        Ok(TypeMut(
            self.type_borrow_impl(strand.vm())
                .borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?,
        ))
    }

    /// Runtime exclusive borrow of `T::Type`, panicking on failure.
    pub fn borrow_mut_unwrap(&self, vm: &Vm<'v>) -> TypeMut<'v, T> {
        TypeMut(
            self.type_borrow_impl(vm)
                .borrow_mut()
                .expect("conflicting borrow"),
        )
    }

    /// Get immutable annex
    pub fn annex(&self, vm: &Vm<'v>) -> &'v T::TypeAnnex {
        &self.type_borrow_impl(vm).annex().inner
    }
}

/// Type-erased async method handler stored in the vtable.
pub(crate) struct MethodHandler<'v> {
    /// Closure allocated with `alias::Box` (no noalias LLVM attr — safe for shared `&` refs).
    closure: NonNull<()>,
    free: unsafe fn(NonNull<()>),
    /// Function pointer using the same implied `'v: 'a` lifetime trick as protocol glue.
    #[expect(clippy::type_complexity)]
    call: for<'a, 's> unsafe fn(
        closure: NonNull<()>,
        header: NonNull<protocol::Header>,
        delegator: Option<&'a Value<'v>>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Pinned<'v, 's, 'a, ()>,
}

impl<'v> MethodHandler<'v> {
    /// Call the method handler.
    ///
    /// # Safety
    /// `header` must point to a live GC object of the type this handler was registered for.
    pub(crate) unsafe fn call<'a, 's>(
        &self,
        header: NonNull<protocol::Header>,
        delegator: Option<&'a Value<'v>>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Pinned<'v, 's, 'a, ()> {
        unsafe { (self.call)(self.closure, header, delegator, strand, args, out) }
    }
}

impl<'v> Drop for MethodHandler<'v> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.closure) }
    }
}

/// Type-erased sync field handler (getter or setter) stored in the vtable.
pub(crate) struct FieldHandler<'v> {
    closure: NonNull<()>,
    free: unsafe fn(NonNull<()>),
    #[expect(clippy::type_complexity)]
    call: for<'a, 's> unsafe fn(
        closure: NonNull<()>,
        header: NonNull<protocol::Header>,
        delegator: Option<&'a Value<'v>>,
        strand: &'a mut Strand<'v, 's>,
        slot: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()>,
}

impl<'v> FieldHandler<'v> {
    /// Call the field handler (getter or setter).
    ///
    /// # Safety
    /// `header` must point to a live GC object of the type this handler was registered for.
    pub(crate) unsafe fn call<'a, 's>(
        &self,
        header: NonNull<protocol::Header>,
        delegator: Option<&'a Value<'v>>,
        strand: &'a mut Strand<'v, 's>,
        slot: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe { (self.call)(self.closure, header, delegator, strand, slot) }
    }
}

impl<'v> Drop for FieldHandler<'v> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.closure) }
    }
}

/// A single entry in the unified namespace table for an `Object` or type-object vtable.
pub(crate) enum Entry<'v> {
    /// An async method handler.
    Method(MethodHandler<'v>),
    /// A sync getter handler.
    Getter(FieldHandler<'v>),
    /// A sync setter handler.
    Setter(FieldHandler<'v>),
    /// A getter + setter pair for the same field name.
    Property(FieldHandler<'v>, FieldHandler<'v>),
    /// Delegate to the supertype at the given index in `TypeAnnexInner::supertypes`.
    Delegate(usize, MemberKind),
}

/// Extension vtable for `Object` types: protocol vtable + sorted namespace entries.
///
/// # Safety
/// `repr(C)` with `base` as the first field so that a pointer to `ObjectVtbl<T>` is
/// layout-compatible with a pointer to the base `protocol::Vtbl`. The GC header's vtbl
/// pointer (which always points to `arena::Vtbl` at offset 0) remains valid when the
/// stored vtbl is actually an `ObjectVtbl`.
#[repr(C)]
pub(crate) struct ObjectVtbl<'v> {
    /// Base vtable — must be first field for pointer-cast compatibility with the GC header.
    pub(crate) base: Vtbl<'v>,
    /// Unified namespace entries sorted ascending by `Sym` for binary-search dispatch.
    pub(crate) entries: alias::Box<[(Sym<'v, 'v>, Entry<'v>)]>,
    /// Index into `Vm::type_singletons` for the type object singleton for this type.
    pub(crate) singleton_idx: usize,
    /// The [`ErrorKind`] instances of this type report, derived at registration
    /// time from the first `std` error variant among its nominal supertypes.
    ///
    /// A native type cannot hold a boxed error's representation, so it declares
    /// its kind here instead; see [`crate::error::Error::kind`].
    pub(crate) error_kind: Option<ErrorKind>,
}

impl<'v> ObjectVtbl<'v> {
    #[inline(never)]
    fn entry(&self, sym: Sym<'v, '_>) -> Option<&Entry<'v>> {
        self.entries
            .binary_search_by_key(&sym, |&(s, _)| s)
            .ok()
            .map(|i| &self.entries[i].1)
    }

    #[inline(never)]
    fn entry_with_sym(&self, sym: Sym<'v, '_>) -> Option<(Sym<'v, 'v>, &Entry<'v>)> {
        self.entries
            .binary_search_by_key(&sym, |&(s, _)| s)
            .ok()
            .map(|i| (self.entries[i].0, &self.entries[i].1))
    }
}

unsafe impl<'v> Upcast<Vtbl<'v>> for ObjectVtbl<'v> {}
unsafe impl<'v> Upcast<arena::Vtbl> for ObjectVtbl<'v> {}

struct FinishResult<'v, 'a> {
    vm: &'a mut Register<'v>,
    inst_vtbl: NonNull<ObjectVtbl<'v>>,
    type_vtbl: NonNull<TypeVtbl<'v>>,
    singleton_idx: usize,
    supertypes: Vec<Value<'v>>,
    nominal_supertypes: Vec<Value<'v>>,
}

/// Non-generic inner state of [`TypeBuilder`].
///
/// Holds all the Vec fields and builder reference. Methods on this type are
/// monomorphic, avoiding duplication across the ~52 `Object` types.
pub struct TypeBuilderInner<'v, 'a> {
    pub(crate) vm: &'a mut Register<'v>,
    pub(crate) entries: Vec<(Sym<'v, 'v>, Entry<'v>)>,
    pub(crate) type_entries: Vec<(Sym<'v, 'v>, Entry<'v>)>,
    pub(crate) supertypes: Vec<Value<'v>>,
    pub(crate) nominal_supertypes: Vec<Value<'v>>,
}

impl<'v, 'a> Deref for TypeBuilderInner<'v, 'a> {
    type Target = Register<'v>;
    fn deref(&self) -> &Register<'v> {
        self.vm
    }
}

impl<'v, 'a> DerefMut for TypeBuilderInner<'v, 'a> {
    fn deref_mut(&mut self) -> &mut Register<'v> {
        self.vm
    }
}

impl<'v, 'a> TypeBuilderInner<'v, 'a> {
    fn new(vm: &'a mut Register<'v>) -> Self {
        Self {
            vm,
            entries: Vec::new(),
            type_entries: Vec::new(),
            supertypes: Vec::new(),
            nominal_supertypes: Vec::new(),
        }
    }

    #[inline(never)]
    fn push_entry(&mut self, name: &str, entry: Entry<'v>) {
        self.entries.push((self.vm.sym(name), entry));
    }

    #[inline(never)]
    fn push_type_entry(&mut self, name: &str, entry: Entry<'v>) {
        self.type_entries.push((self.vm.sym(name), entry));
    }

    /// Register an abstract supertype type object.
    ///
    /// # Panics
    ///
    /// Panics if the provided type object does not support inheritance or is not abstract.
    #[inline(never)]
    pub fn supertype(&mut self, supertype: impl Input<'v>) {
        let supertype = Value::from_input(self.vm, supertype);
        if self
            .supertypes
            .iter()
            .any(|existing| existing.repr_eq(self.vm, &supertype))
        {
            return;
        }
        let Some(inspect) = supertype.op_inspect(self.vm) else {
            panic!("native object supertype must support inspect");
        };
        if !inspect.is_abstract {
            panic!(
                "native object supertype must be abstract; a concrete supertype's representation \
                 cannot be inherited by a native type, so use `nominal_supertype` and implement \
                 its interface directly"
            );
        }
        self.supertypes.push(supertype);
    }

    /// Register an abstract nominal supertype used only for subtype checks.
    ///
    /// # Panics
    ///
    /// Panics if the provided type object does not support inspect.
    #[inline(never)]
    pub fn nominal_supertype(&mut self, supertype: impl Input<'v>) {
        let supertype = Value::from_input(self.vm, supertype);
        if self
            .nominal_supertypes
            .iter()
            .any(|existing| existing.repr_eq(self.vm, &supertype))
        {
            return;
        }
        if supertype.op_inspect(self.vm).is_none() {
            panic!("native object nominal supertype must support inspect");
        }
        self.nominal_supertypes.push(supertype);
    }

    /// Synthesize delegate entries from abstract supertypes, sort and merge
    /// all handler tables, register vtbls, and reserve a singleton slot.
    /// Returns `(vm, inst_vtbl, type_vtbl, singleton_idx)`. The caller must
    /// construct and push the singleton into `type_singletons[idx]`.
    #[inline(never)]
    fn finish(
        mut self,
        inst_vtbl_base: protocol::Vtbl<'v>,
        type_vtbl_base: protocol::Vtbl<'v>,
    ) -> FinishResult<'v, 'a> {
        // Synthesize delegated entries from abstract supertypes.
        for (supertype_idx, supertype) in self.supertypes.iter().enumerate() {
            let inspect = supertype
                .op_inspect(self.vm)
                .expect("native object supertype must support inspect");
            assert!(
                inspect.is_abstract,
                "native object supertype must be abstract"
            );
            for member in inspect.members {
                // Safety: all supertypes available at native type registration time
                // should have static symbols.
                let sym = unsafe { member.sym.into_static_scope_unchecked() };
                // Local entries win; first matching supertype wins.
                if !self.entries.iter().any(|(entry_sym, _)| *entry_sym == sym) {
                    self.entries
                        .push((sym, Entry::Delegate(supertype_idx, member.kind)));
                }
            }
        }

        // Derive the reported error kind from the nominal supertypes rather than
        // taking it as a separate builder setting, so it cannot drift from the
        // subtype relation the same declaration establishes.
        let error_kind = self.nominal_supertypes.iter().find_map(|supertype| {
            let variant = supertype.downcast_ref(self.vm.builtin_types().error_variant_type)?;
            let kind = variant.get().0;
            assert!(
                kind != ErrorKind::Abort,
                "std.AbortError cannot be a nominal supertype"
            );
            Some(kind)
        });

        let entries = merge_entries(self.entries);
        let type_entries = merge_entries(self.type_entries);

        let mut members = vec![
            Member::method(Sym::well_known(sym::INIT_METHOD)),
            Member::method(Sym::well_known(sym::STR_METHOD)),
            Member::method(Sym::well_known(sym::DBG_METHOD)),
            Member::method(Sym::well_known(sym::FMT_METHOD)),
            Member::method(Sym::well_known(sym::BOOL_METHOD)),
            Member::method(Sym::well_known(sym::HASH_METHOD)),
            Member::method(Sym::well_known(sym::EQ_METHOD)),
            Member::method(Sym::well_known(sym::LT_METHOD)),
            Member::method(Sym::well_known(sym::NEG_METHOD)),
            Member::method(Sym::well_known(sym::BNOT_METHOD)),
            Member::method(Sym::well_known(sym::ADD_METHOD)),
            Member::method(Sym::well_known(sym::SUB_METHOD)),
            Member::method(Sym::well_known(sym::RSUB_METHOD)),
            Member::method(Sym::well_known(sym::MUL_METHOD)),
            Member::method(Sym::well_known(sym::DIV_METHOD)),
            Member::method(Sym::well_known(sym::RDIV_METHOD)),
            Member::method(Sym::well_known(sym::EDIV_METHOD)),
            Member::method(Sym::well_known(sym::REDIV_METHOD)),
            Member::method(Sym::well_known(sym::MOD_METHOD)),
            Member::method(Sym::well_known(sym::RMOD_METHOD)),
            Member::method(Sym::well_known(sym::BAND_METHOD)),
            Member::method(Sym::well_known(sym::BOR_METHOD)),
            Member::method(Sym::well_known(sym::BXOR_METHOD)),
        ];
        members.extend(entries.iter().map(|(sym, entry)| {
            let kind = match entry {
                Entry::Method(_) => MemberKind::Method,
                Entry::Getter(_) => MemberKind::Getter,
                Entry::Setter(_) => MemberKind::Setter,
                Entry::Property(_, _) => MemberKind::Property,
                Entry::Delegate(_, kind) => *kind,
            };
            Member::new(*sym, kind)
        }));
        let mut type_members = type_entries
            .iter()
            .map(|(sym, entry)| {
                let kind = match entry {
                    Entry::Method(_) => MemberKind::Method,
                    Entry::Getter(_) => MemberKind::Getter,
                    Entry::Setter(_) => MemberKind::Setter,
                    Entry::Property(_, _) => MemberKind::Property,
                    Entry::Delegate(_, kind) => *kind,
                };
                Member::new(*sym, kind)
            })
            .collect::<Vec<_>>();
        // Only the type object's own surface belongs here. The instance namespace is
        // reachable through the type object as unbound methods, but those are not class
        // members and must not be inherited by a Do class as such.
        for member in [
            Member::method(Sym::well_known(sym::VERBATIM_METHOD)),
            Member::method(Sym::well_known(sym::STR_METHOD)),
            Member::method(Sym::well_known(sym::DBG_METHOD)),
            Member::method(Sym::well_known(sym::CALL_METHOD)),
        ] {
            if !type_members
                .iter()
                .any(|existing| existing.sym == member.sym)
            {
                type_members.push(member);
            }
        }

        let idx = self.vm.inner.type_singletons.len();

        let inst_vtbl = self.vm.inner.types.register(ObjectVtbl {
            base: inst_vtbl_base,
            entries: entries.into(),
            singleton_idx: idx,
            error_kind,
        });

        if error_kind.is_some() {
            self.vm.inner.error_kind_vtbls.push(inst_vtbl);
        }

        let type_vtbl = self.vm.inner.types.register(TypeVtbl {
            base: type_vtbl_base,
            entries: type_entries.into(),
            members: members.into(),
            type_members: type_members.into(),
            inst_vtbl,
        });

        FinishResult {
            vm: self.vm,
            inst_vtbl,
            type_vtbl,
            singleton_idx: idx,
            supertypes: self.supertypes,
            nominal_supertypes: self.nominal_supertypes,
        }
    }
}

/// Builder for registering methods and field accessors on an [`Object`] type during VM setup.
///
/// Returned by [`Register::build_type`](crate::vm::Register::build_type) after passing through
/// [`Object::build`], and consumed by [`TypeBuilder::build`] to create the native type.
/// Implements [`Deref`]/[`DerefMut`] to [`Register`] so that `Register` methods
/// (e.g. [`Register::sym`]) can be called directly when capturing symbols in closures.
pub struct TypeBuilder<'v, 'a, T: Object<'v>> {
    pub(crate) inner: TypeBuilderInner<'v, 'a>,
    type_value: T::Type,
    type_annex: T::TypeAnnex,
    _phantom: PhantomData<T>,
}

impl<'v, 'a, T: Object<'v>> Deref for TypeBuilder<'v, 'a, T> {
    type Target = Register<'v>;
    fn deref(&self) -> &Register<'v> {
        self.inner.vm
    }
}

impl<'v, 'a, T: Object<'v>> DerefMut for TypeBuilder<'v, 'a, T> {
    fn deref_mut(&mut self) -> &mut Register<'v> {
        self.inner.vm
    }
}

impl<'v, 'a, T: Object<'v>> Alloc<'v> for TypeBuilder<'v, 'a, T> {
    fn alloc_vm(&mut self, sealed: crate::vm::private::Sealed) -> &'v Vm<'v> {
        self.inner.vm.alloc_vm(sealed)
    }
}

impl<'v, 'a, T: Object<'v>> TypeBuilder<'v, 'a, T> {
    pub(crate) fn new(
        vm: &'a mut Register<'v>,
        type_value: T::Type,
        type_annex: T::TypeAnnex,
    ) -> Self {
        Self {
            inner: TypeBuilderInner::new(vm),
            type_value,
            type_annex,
            _phantom: PhantomData,
        }
    }

    /// Register an abstract supertype type object.
    ///
    /// # Panics
    ///
    /// Panics if the provided type object does not support inheritance or is not abstract.
    pub fn supertype(mut self, supertype: impl Input<'v>) -> Self {
        self.inner.supertype(supertype);
        self
    }

    /// Register an abstract nominal supertype used only for subtype checks.
    ///
    /// # Panics
    ///
    /// Panics if the provided type object does not support inspect.
    pub fn nominal_supertype(mut self, supertype: impl Input<'v>) -> Self {
        self.inner.nominal_supertype(supertype);
        self
    }

    /// Register a method with the given name.
    ///
    /// The closure receives an [`Instance`] receiver, the current [`Strand`], call
    /// arguments, and an output slot..
    pub fn method<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> AsyncFn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Args<'v, 'b>,
                Slot<'v, 'b>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.inner.push_entry(
            name,
            Entry::Method(MethodHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: call_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a getter with the given name.
    ///
    /// The closure receives an [`Instance`] receiver, the current [`Strand`], and an
    /// output slot.
    pub fn get<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> Fn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Slot<'v, 'b>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.inner.push_entry(
            name,
            Entry::Getter(FieldHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: field_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a setter with the given name.
    ///
    /// The closure receives an [`Instance`] receiver, the current [`Strand`], and a
    /// value slot.
    pub fn set<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> Fn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Slot<'v, 'b>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.inner.push_entry(
            name,
            Entry::Setter(FieldHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: field_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a method with scratch [`Slot`]s for temporary values.
    ///
    /// Like [`method`](Self::method) but the closure also receives an array of scratch slots.
    pub fn method_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> AsyncFn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Args<'v, 'b>,
                Slot<'v, 'b>,
                [Slot<'v, 'b>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.method(name, async move |this, strand, args, out| {
            strand
                .with_slots(async |strand, slots| f(this, strand, args, out, slots).await)
                .await
        })
    }

    /// Register a getter with scratch [`Slot`]s for temporary values.
    ///
    /// Like [`get`](Self::get) but the closure also receives an array of scratch slots.
    pub fn get_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> Fn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Slot<'v, 'b>,
                [Slot<'v, 'b>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.get(name, move |this, strand, out| {
            strand.with_slots_sync(|strand, slots| f(this, strand, out, slots))
        })
    }

    /// Register a setter with scratch [`Slot`]s for temporary values.
    ///
    /// Like [`set`](Self::set) but the closure also receives an array of scratch slots.
    pub fn set_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> Fn(
                Instance<'v, 'b, T>,
                &mut Strand<'v, 's>,
                Slot<'v, 'b>,
                [Slot<'v, 'b>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.set(name, move |this, strand, val| {
            strand.with_slots_sync(|strand, slots| f(this, strand, val, slots))
        })
    }

    /// Register a type-level method with the given name.
    ///
    /// The closure receives a [`Type`] handle (the type object singleton), the current
    /// [`Strand`], call arguments, and an output slot.
    pub fn type_method<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'s> AsyncFn(
                Type<'v, T>,
                &mut Strand<'v, 's>,
                Args<'v, '_>,
                Slot<'v, '_>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.inner.push_type_entry(
            name,
            Entry::Method(MethodHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: type_call_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a type-level getter with the given name.
    pub fn type_get<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'s> Fn(Type<'v, T>, &mut Strand<'v, 's>, Slot<'v, '_>) -> Result<'v, 's, ()> + 'v,
    {
        self.inner.push_type_entry(
            name,
            Entry::Getter(FieldHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: type_field_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a type-level setter with the given name.
    pub fn type_set<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'s> Fn(Type<'v, T>, &mut Strand<'v, 's>, Slot<'v, '_>) -> Result<'v, 's, ()> + 'v,
    {
        self.inner.push_type_entry(
            name,
            Entry::Setter(FieldHandler {
                closure: alias::Box::into_non_null(alias::Box::new(f)).cast(),
                free: free_glue::<F>,
                call: type_field_glue::<T, F>,
            }),
        );
        self
    }

    /// Register a type-level method with scratch [`Slot`]s for temporary values.
    pub fn type_method_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'s> AsyncFn(
                Type<'v, T>,
                &mut Strand<'v, 's>,
                Args<'v, '_>,
                Slot<'v, '_>,
                [Slot<'v, '_>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.type_method(name, async move |this, strand, args, out| {
            strand
                .with_slots(async |strand, slots| f(this, strand, args, out, slots).await)
                .await
        })
    }

    /// Register a type-level getter with scratch [`Slot`]s for temporary values.
    pub fn type_get_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'s> Fn(
                Type<'v, T>,
                &mut Strand<'v, 's>,
                Slot<'v, '_>,
                [Slot<'v, '_>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.type_get(name, move |this, strand, out| {
            strand.with_slots_sync(|strand, slots| f(this, strand, out, slots))
        })
    }

    /// Register a type-level setter with scratch [`Slot`]s for temporary values.
    pub fn type_set_with_slots<const N: usize, F>(self, name: &str, f: F) -> Self
    where
        F: for<'s> Fn(
                Type<'v, T>,
                &mut Strand<'v, 's>,
                Slot<'v, '_>,
                [Slot<'v, '_>; N],
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        self.type_set(name, move |this, strand, val| {
            strand.with_slots_sync(|strand, slots| f(this, strand, val, slots))
        })
    }

    /// Consume the builder, register all vtables, allocate the type singleton, and
    /// return the [`Type`] handle.
    pub fn build(self) -> Type<'v, T> {
        let inst_vtbl_base = protocol::Vtbl::new::<ObjectWrap<'v, T>>();
        let type_vtbl_base = protocol::Vtbl::new::<TypeObjectWrap<'v, T>>();
        let result = self.inner.finish(inst_vtbl_base, type_vtbl_base);

        let ty = Type {
            vtbl: unsafe { TypeHandle::new(result.inst_vtbl.cast()) },
            type_vtbl: unsafe { TypeHandle::new(result.type_vtbl.cast()) },
            singleton_idx: result.singleton_idx,
            class_instance: result.vm.inner.builtin_types().class_instance,
        };

        let singleton = Value::from_object(protocol::GcObj::new_annex(
            result.vm.inner.arena(),
            ty.type_vtbl,
            TypeObjectWrap(self.type_value, PhantomData),
            TypeAnnexInner {
                inner: self.type_annex,
                supertypes: result.supertypes.into(),
                nominal_supertypes: result.nominal_supertypes.into(),
            },
        ));
        result.vm.inner.type_singletons.push(singleton);

        ty
    }
}

/// Sort entries by Sym, then merge adjacent Getter+Setter (same Sym) into Property.
#[inline(never)]
fn merge_entries<'v>(mut entries: Vec<(Sym<'v, 'v>, Entry<'v>)>) -> Vec<(Sym<'v, 'v>, Entry<'v>)> {
    entries.sort_by_key(|&(sym, _)| sym);
    let mut merged: Vec<(Sym<'_, '_>, Entry<'_>)> = Vec::with_capacity(entries.len());
    for (sym, entry) in entries {
        if let Some((last_sym, last_entry)) = merged.last_mut()
            && *last_sym == sym
        {
            // Merge Getter + Setter into Property
            let prev = std::mem::replace(last_entry, Entry::Delegate(0, MemberKind::Method)); // placeholder
            *last_entry = match (prev, entry) {
                (Entry::Getter(g), Entry::Setter(s)) | (Entry::Setter(s), Entry::Getter(g)) => {
                    Entry::Property(g, s)
                }
                _ => panic!("duplicate entry for sym {sym:?}"),
            };
            continue;
        }
        merged.push((sym, entry));
    }
    merged
}

/// Drop glue: runs the destructor for `F` then frees the allocation.
unsafe fn free_glue<F>(ptr: NonNull<()>) {
    unsafe { drop(alias::Box::<F>::from_non_null(ptr.cast())) }
}

fn instance_supertype<'v, 'a, T: Object<'v>>(
    this: Recv<'v, 'a, ObjectWrap<'v, T>>,
    vm: &Vm<'v>,
    idx: usize,
) -> &'a Value<'v> {
    let Case::Object(type_obj) = this.singleton(vm).case() else {
        unreachable!();
    };
    // FIXME: awkward, could use an "unchecked downcast" method on borrows
    let type_obj = unsafe {
        GcObjBorrow::<'v, 'v, TypeObjectWrap<'v, T>>::from_raw(type_obj.into_raw().cast())
    };
    &type_obj.annex().supertypes[idx]
}

/// Call glue: reconstructs a typed `Instance<T>` from the raw header and calls `F`.
///
/// The signature mirrors the protocol vtable glue functions: `'v` is the GC arena lifetime,
/// `'a` and `'s` are the call-site lifetimes quantified by the outer `for<'a, 's>`.
unsafe fn call_glue<'v, 'a, 's, T, F>(
    closure: NonNull<()>,
    header: NonNull<protocol::Header>,
    delegator: Option<&'a Value<'v>>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Pinned<'v, 's, 'a, ()>
where
    T: Object<'v>,
    F: for<'b, 'r> AsyncFn(
            Instance<'v, 'b, T>,
            &mut Strand<'v, 'r>,
            Args<'v, 'b>,
            Slot<'v, 'b>,
        ) -> Result<'v, 'r, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    let inst = Instance {
        receiver: unsafe { gc::Borrow::from_raw(header.cast()) },
        delegator,
    };
    strand.pin_future_call(async move |strand| f(inst, strand, args, out).await)
}

/// Field glue: reconstructs a typed `Instance<T>` from the raw header and calls `F`.
unsafe fn field_glue<'v, 'a, 's, T, F>(
    closure: NonNull<()>,
    header: NonNull<protocol::Header>,
    delegator: Option<&'a Value<'v>>,
    strand: &'a mut Strand<'v, 's>,
    slot: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    T: Object<'v>,
    F: for<'b, 'r> Fn(Instance<'v, 'b, T>, &mut Strand<'v, 'r>, Slot<'v, 'b>) -> Result<'v, 'r, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    let inst = Instance {
        receiver: unsafe { gc::Borrow::from_raw(header.cast()) },
        delegator,
    };
    f(inst, strand, slot)
}

// ──────────────────────────────────────────────────────────────────────────────
// Type object support
// ──────────────────────────────────────────────────────────────────────────────

/// GC-managed wrapper for the type object singleton's borrow-checked main struct (`T::Type`).
///
/// Analogous to [`ObjectWrap`] for instances.  Its [`gc::Collect::Annex`] is
/// [`TypeAnnexInner`], which holds the immutable parts of the type object.
pub(crate) struct TypeObjectWrap<'v, T: Object<'v>>(
    pub(crate) T::Type,
    pub(crate) PhantomData<&'v mut &'v ()>,
);

/// Immutable annex for a type object singleton.  Never runtime-borrow-checked;
/// accessible directly via [`Type::annex`].  The `Type<'v,T>` itself can be
/// reconstituted from the type object header via [`Type::from_type_header`].
pub(crate) struct TypeAnnexInner<'v, T: Object<'v>> {
    pub(crate) inner: T::TypeAnnex,
    pub(crate) supertypes: alias::Box<[Value<'v>]>,
    pub(crate) nominal_supertypes: alias::Box<[Value<'v>]>,
}

impl<'v, T: Object<'v>> gc::Annex for TypeAnnexInner<'v, T> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for supertype in self.supertypes.iter() {
            supertype.accept(visit)?;
        }
        for supertype in self.nominal_supertypes.iter() {
            supertype.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&self) {
        // TypeObjectWrap::CYCLIC = false, so clear is never called on type objects
    }
}

unsafe impl<'v, T: Object<'v>> Collect for TypeObjectWrap<'v, T> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = false;
    type Annex = TypeAnnexInner<'v, T>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v, 'a, T: Object<'v>> Recv<'v, 'a, TypeObjectWrap<'v, T>> {
    fn vtbl(&self) -> &'a TypeVtbl<'v> {
        unsafe { self.vtbl_downcast_unchecked::<TypeVtbl<'v>>() }
    }

    fn inst_vtbl(&self) -> &'a ObjectVtbl<'v> {
        unsafe { self.vtbl().inst_vtbl.as_ref() }
    }

    fn entry(&self, sym: Sym<'v, '_>) -> Option<&'a Entry<'v>> {
        self.vtbl().entry(sym)
    }

    fn inst_entry(&self, sym: Sym<'v, '_>) -> Option<&'a Entry<'v>> {
        self.inst_vtbl().entry(sym)
    }

    /// Returns a reference to the type singleton [`Value`] for this type.
    fn singleton<'b>(&self, vm: &'b Vm<'v>) -> &'b Value<'v> {
        &vm.type_singletons[self.inst_vtbl().singleton_idx]
    }

    fn ty(&self, vm: &Vm<'v>) -> Type<'v, T> {
        // SAFETY: `self` is a live `TypeObjectWrap<'v, T>` receiver.
        unsafe { Type::<T>::from_type_header(self.as_header().cast(), vm) }
    }
}

impl<'v, T: Object<'v>> Protocol<'v> for TypeObjectWrap<'v, T> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        // The type of a type object is the "type" singleton.
        Output::set(strand, out, &strand.singletons().type_obj);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type {}.{}>", T::MODULE, T::NAME)
    }

    fn op_inspect<'a>(this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: &this.vtbl().members,
            type_members: &this.vtbl().type_members,
        })
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ty = this.ty(strand.vm());
        Strand::async_for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            None,
            async |strand| T::new(ty, strand, args, out).await,
        )
        .await
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some((sym, entry)) = this.vtbl().entry_with_sym(method) {
            let name = sym.as_str(strand);
            Strand::async_for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed(name)),
                async |strand| match entry {
                    Entry::Method(handler) => unsafe {
                        handler
                            .call(this.as_header(), this.delegator(), strand, args, out)
                            .await
                    },
                    _ => Err(Error::field(strand, method)),
                },
            )
            .await
        } else if method.tag() == sym::INIT_METHOD {
            Strand::async_for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed("(init)")),
                async |strand| {
                    let mut out = out;
                    let ([inst], [], trailing) = unpack!(strand, args, 1, 0, ...)?;
                    let ty = this.ty(strand.vm());
                    T::new(ty, strand, trailing, Slot::reborrow(&mut out)).await?;
                    let native = out.take();
                    let singleton = this.singleton(strand.vm());
                    inst.op_fill(strand, singleton, native)
                },
            )
            .await
        } else if method.tag() == sym::GET_METHOD {
            let ([field], []) = unpack!(strand, args, 1, 0)?;
            let field = field
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
            Self::op_get(this, strand, field, out)
        } else if method.tag() == sym::SET_METHOD {
            let ([field, value], []) = unpack!(strand, args, 2, 0)?;
            let field = field
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
            Self::op_set(this, strand, field, value)
        } else if let Some((sym, entry)) = this.inst_vtbl().entry_with_sym(method) {
            let name = sym.as_str(strand);
            Strand::async_for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed(name)),
                async |strand| match entry {
                    Entry::Method(handler) => {
                        // Unbound instance method: downcast the explicit self argument to
                        // the precise native type (handling Do subclasses via native slots)
                        // and invoke the handler directly.
                        let ([inst], [], trailing) = unpack!(strand, args, 1, 0, ...)?;
                        let inst = inst
                            .downcast_native(strand, unsafe {
                                TypeHandle::<ObjectWrap<'v, T>>::new(this.vtbl().inst_vtbl.cast())
                            })
                            .ok_or_else(|| {
                                Error::type_error(
                                    strand,
                                    format!("expected {}.{}", T::MODULE, T::NAME),
                                )
                            })?
                            .into_raw()
                            .cast();
                        unsafe { handler.call(inst, None, strand, trailing, out).await }
                    }
                    _ => Err(Error::field(strand, method)),
                },
            )
            .await
        } else {
            // Fall through to type_mcall_fallback for protocol/special
            // methods (str, dbg, bool, hash, arithmetic, comparison, etc.).
            // It will error for unsupported operations.
            if matches!(
                method.tag(),
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
            ) {
                let singleton = this.singleton(strand.vm());
                type_mcall_fallback(strand, singleton, method, args, out).await
            } else {
                T::type_method(this.ty(strand.vm()), strand, method, args, out).await
            }
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::GET_METHOD | sym::SET_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                return Ok(());
            }
            _ => (),
        }

        // Check type-level entries first.
        if let Some(entry) = this.entry(field) {
            return match entry {
                Entry::Getter(handler) | Entry::Property(handler, _) => Strand::for_native_frame(
                    strand,
                    Cow::Borrowed(T::MODULE),
                    Cow::Borrowed(T::NAME),
                    Some(Cow::Borrowed("(get)")),
                    |strand| unsafe {
                        handler.call(this.as_header(), this.delegator(), strand, out)
                    },
                ),
                Entry::Method(_) => {
                    BoundMethod::create(strand, &this, field, out);
                    Ok(())
                }
                Entry::Setter(_) | Entry::Delegate(_, _) => Err(Error::field(strand, field)),
            };
        }
        // Check instance-level entries (unbound methods/getters).
        if let Some(entry) = this.inst_entry(field) {
            return match entry {
                Entry::Method(_)
                | Entry::Getter(_)
                | Entry::Property(_, _)
                | Entry::Delegate(_, _) => {
                    BoundMethod::create(strand, &this, field, out);
                    Ok(())
                }
                Entry::Setter(_) => Err(Error::field(strand, field)),
            };
        }
        // Special/protocol methods are always callable.
        if matches!(
            field.tag(),
            sym::INIT_METHOD
                | sym::STR_METHOD
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
        ) {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            Strand::for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed("(get)")),
                |strand| T::type_get(this.ty(strand.vm()), strand, field, out),
            )
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(entry) = this.entry(field) {
            match entry {
                Entry::Setter(handler) | Entry::Property(_, handler) => Strand::for_native_frame(
                    strand,
                    Cow::Borrowed(T::MODULE),
                    Cow::Borrowed(T::NAME),
                    Some(Cow::Borrowed("(set)")),
                    |strand| unsafe {
                        handler.call(this.as_header(), this.delegator(), strand, value)
                    },
                ),
                Entry::Getter(_) => Err(Error::immutable(strand)),
                _ => Err(Error::field(strand, field)),
            }
        } else {
            Strand::for_native_frame(
                strand,
                Cow::Borrowed(T::MODULE),
                Cow::Borrowed(T::NAME),
                Some(Cow::Borrowed("(set)")),
                |strand| T::type_set(this.ty(strand.vm()), strand, field, value),
            )
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(index)")),
            |strand| T::type_index(this.ty(strand.vm()), strand, index, out),
        )
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        use std::ptr;
        ptr::hash(this.receiver.into_raw().as_ptr(), hasher);
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        Ok(Value::from_bool(other.repr_eq(strand, this)))
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        if supertype.repr_eq(strand, &this) || supertype.repr_eq(strand, TypeObject::Value) {
            return true;
        }
        let ty = this.ty(strand.vm());
        let annex = ty.type_borrow_impl(strand).annex();
        annex
            .supertypes
            .iter()
            .chain(annex.nominal_supertypes.iter())
            .any(|sup| sup.op_subtype(strand, supertype))
    }
}

/// Extension vtable for type object singletons.
///
/// # Safety
/// `repr(C)` with `base` as first field for pointer-cast compatibility with the GC header.
#[repr(C)]
pub(crate) struct TypeVtbl<'v> {
    /// Base vtable — must be first field for pointer-cast compatibility.
    pub(crate) base: Vtbl<'v>,
    /// Unified namespace entries sorted ascending by `Sym`.
    pub(crate) entries: alias::Box<[(Sym<'v, 'v>, Entry<'v>)]>,
    /// Stable semantic description of the instance namespace advertised by
    /// the type object protocol.
    pub(crate) members: alias::Box<[Member<'v, 'v>]>,
    /// Stable semantic description of the type-object namespace advertised by
    /// the type object protocol.
    pub(crate) type_members: alias::Box<[Member<'v, 'v>]>,
    /// Reference to the instance vtbl for unbound instance method dispatch.
    pub(crate) inst_vtbl: NonNull<ObjectVtbl<'v>>,
}

impl<'v> TypeVtbl<'v> {
    #[inline(never)]
    fn entry(&self, sym: Sym<'v, '_>) -> Option<&Entry<'v>> {
        self.entries
            .binary_search_by_key(&sym, |&(s, _)| s)
            .ok()
            .map(|i| &self.entries[i].1)
    }

    #[inline(never)]
    fn entry_with_sym(&self, sym: Sym<'v, '_>) -> Option<(Sym<'v, 'v>, &Entry<'v>)> {
        self.entries
            .binary_search_by_key(&sym, |&(s, _)| s)
            .ok()
            .map(|i| (self.entries[i].0, &self.entries[i].1))
    }
}

unsafe impl<'v> Upcast<Vtbl<'v>> for TypeVtbl<'v> {}
unsafe impl<'v> Upcast<arena::Vtbl> for TypeVtbl<'v> {}

/// Shared borrow guard for the type object singleton's `T::Type` main struct.
///
/// Analogous to [`Ref`] for instances.
pub struct TypeRef<'v, T: Object<'v>>(gc::Ref<'v, 'v, protocol::Header, TypeObjectWrap<'v, T>>);

impl<'v, T: Object<'v>> Deref for TypeRef<'v, T> {
    type Target = T::Type;
    #[inline]
    fn deref(&self) -> &T::Type {
        &self.0.0
    }
}

impl<'v, T: Object<'v>> AsRef<T::Type> for TypeRef<'v, T> {
    #[inline]
    fn as_ref(&self) -> &T::Type {
        self
    }
}

/// Exclusive borrow guard for the type object singleton's `T::Type`.
///
/// Analogous to [`Mut`] for instances.
pub struct TypeMut<'v, T: Object<'v>>(gc::Mut<'v, 'v, protocol::Header, TypeObjectWrap<'v, T>>);

impl<'v, T: Object<'v>> Deref for TypeMut<'v, T> {
    type Target = T::Type;
    #[inline]
    fn deref(&self) -> &T::Type {
        &self.0.0
    }
}

impl<'v, T: Object<'v>> DerefMut for TypeMut<'v, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T::Type {
        &mut self.0.0
    }
}

impl<'v, T: Object<'v>> AsRef<T::Type> for TypeMut<'v, T> {
    #[inline]
    fn as_ref(&self) -> &T::Type {
        self
    }
}

/// Call glue for type-level method closures: reconstructs `Type<T>` from the raw header.
unsafe fn type_call_glue<'v, 'a, 's, T: Object<'v>, F>(
    closure: NonNull<()>,
    header: NonNull<protocol::Header>,
    _delegator: Option<&'a Value<'v>>,
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Pinned<'v, 's, 'a, ()>
where
    F: for<'ss> AsyncFn(
            Type<'v, T>,
            &mut Strand<'v, 'ss>,
            Args<'v, '_>,
            Slot<'v, '_>,
        ) -> Result<'v, 'ss, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    // Reconstruct Type<'v,T> from the type object header.
    let ty = unsafe { Type::<T>::from_type_header(header.cast(), strand.vm()) };
    strand.pin_future_call(async move |strand| f(ty, strand, args, out).await)
}

/// Field glue for type-level closures: reconstructs `Type<T>` from the raw header.
unsafe fn type_field_glue<'v, 'a, 's, T: Object<'v>, F>(
    closure: NonNull<()>,
    header: NonNull<protocol::Header>,
    _delegator: Option<&'a Value<'v>>,
    strand: &'a mut Strand<'v, 's>,
    slot: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    F: for<'ss> Fn(Type<'v, T>, &mut Strand<'v, 'ss>, Slot<'v, '_>) -> Result<'v, 'ss, ()> + 'v,
{
    let f = unsafe { &*closure.cast::<F>().as_ptr() };
    // Reconstruct Type<'v, T> from the type object header.
    let ty = unsafe { Type::<T>::from_type_header(header.cast(), strand.vm()) };
    f(ty, strand, slot)
}

#[cfg(test)]
mod tests {
    use dolang_bytecode::Variadic;

    use crate::{
        error::ErrorKind,
        sig, sym,
        test_support::{args_from_slots, with_builder},
        value::TypeObject,
        vm::{Builder, Stateful},
    };

    use super::*;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// A native object type that overrides none of `Object`'s methods. Used to
    /// exercise the trait's default implementations, driven through
    /// `ObjectWrap`'s `Protocol` dispatch (`op_*`) so both the dispatch glue
    /// and the default bodies get covered together.
    struct Fixture;

    impl<'v> Object<'v> for Fixture {
        const MODULE: &'v str = "test";
        const NAME: &'v str = "Fixture";
        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

        async fn method<'a, 's>(
            this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            _method: Sym<'v, 'a>,
            args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> Result<'v, 's, ()> {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Output::set(strand, out, this);
            Ok(())
        }
    }

    /// A native object type with a representative mix of `TypeBuilder`
    /// registrations (method, getter, setter, property, scratch-slot method,
    /// type-method, type-getter, type-setter), one GC slot, and non-trivial
    /// `Annex`/`Type`/`TypeAnnex`. Used to exercise `TypeBuilder`, the
    /// registered-entry dispatch paths in `op_mcall`/`op_get`/`op_set`, and
    /// `Instance`/`Ref`/`Mut`/`Annex`/`Type` accessors.
    struct SlotFixture {
        counter: i64,
    }

    struct SlotAnnex {
        tag: &'static str,
    }

    impl<'v> Object<'v> for SlotFixture {
        const MODULE: &'v str = "test";
        const NAME: &'v str = "SlotFixture";
        const SLOTS: usize = 1;
        type Annex = SlotAnnex;
        type Type = i64;
        type TypeAnnex = i64;

        fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
            let absent_sym = builder.sym("absent");
            builder
                .method("receiver", async move |this, strand, args, out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    Output::set(strand, out, this);
                    Ok(())
                })
                .method("bump", async move |this, strand, args, out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    let value = {
                        let mut m = this.borrow_mut(strand)?;
                        m.counter += 1;
                        m.counter
                    };
                    Output::set(strand, out, value);
                    Ok(())
                })
                .get("counter", |this, strand, out| {
                    let value = this.borrow(strand)?.counter;
                    Output::set(strand, out, value);
                    Ok(())
                })
                .get("prop", |_this, strand, out| {
                    Output::set(strand, out, 11_i64);
                    Ok(())
                })
                .set("prop", |_this, _strand, _value| Ok(()))
                .set("write_only", |_this, _strand, _value| Ok(()))
                .get("absent", move |this, strand, _out| {
                    this.borrow_mut(strand)?.counter += 1;
                    Err(Error::field(strand, absent_sym))
                })
                .get("bad", |_this, strand, _out| {
                    Err(Error::value(strand, "bad getter"))
                })
                .method_with_slots::<1, _>(
                    "with_slot",
                    async move |_this, strand, args, out, [mut scratch]| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        Output::set(strand, Slot::reborrow(&mut scratch), 5_i64);
                        Output::set(strand, out, &scratch);
                        Ok(())
                    },
                )
                .type_method("make", async move |ty, strand, args, out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    ty.create_with_annex(
                        strand,
                        SlotFixture { counter: 0 },
                        SlotAnnex { tag: "made" },
                        out,
                    );
                    Ok(())
                })
                .type_get("meta", |ty, strand, out| {
                    let value = *ty.borrow(strand)?;
                    Output::set(strand, out, value);
                    Ok(())
                })
                .type_set("meta", |ty, strand, value| {
                    let n = value.to_i64(strand)?;
                    *ty.borrow_mut(strand)? = n;
                    Ok(())
                })
                .type_get("readonly", |_ty, strand, out| {
                    Output::set(strand, out, 5_i64);
                    Ok(())
                })
        }
    }

    /// A native object type that claims an `std` error kind by nominal
    /// supertype, the way an extension's error type does. Used to check that
    /// `Error::kind` classifies its instances by the declared kind rather than
    /// falling back to `Runtime`.
    struct ErrorFixture;

    impl<'v> Object<'v> for ErrorFixture {
        const MODULE: &'v str = "test";
        const NAME: &'v str = "ErrorFixture";
        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

        fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
            builder.nominal_supertype(TypeObject::ValueError)
        }
    }

    struct FixtureState<'v> {
        fixture_ty: Type<'v, Fixture>,
        error_ty: Type<'v, ErrorFixture>,
        slot_ty: Type<'v, SlotFixture>,
        bump_sym: Sym<'v, 'v>,
        receiver_sym: Sym<'v, 'v>,
        counter_sym: Sym<'v, 'v>,
        prop_sym: Sym<'v, 'v>,
        write_only_sym: Sym<'v, 'v>,
        with_slot_sym: Sym<'v, 'v>,
        make_sym: Sym<'v, 'v>,
        meta_sym: Sym<'v, 'v>,
        readonly_sym: Sym<'v, 'v>,
        absent_sym: Sym<'v, 'v>,
        bad_sym: Sym<'v, 'v>,
    }

    struct FixtureStateTag;

    impl<'v> Stateful<'v> for FixtureState<'v> {
        type Tag = FixtureStateTag;
    }

    fn configure(vm: &mut Builder<'_>) {
        let fixture_ty = vm.register_type::<Fixture>();
        let error_ty = vm.register_type::<ErrorFixture>();
        let slot_ty = vm.register_type::<SlotFixture>();
        let bump_sym = vm.sym("bump");
        let receiver_sym = vm.sym("receiver");
        let counter_sym = vm.sym("counter");
        let prop_sym = vm.sym("prop");
        let write_only_sym = vm.sym("write_only");
        let with_slot_sym = vm.sym("with_slot");
        let make_sym = vm.sym("make");
        let meta_sym = vm.sym("meta");
        let readonly_sym = vm.sym("readonly");
        let absent_sym = vm.sym("absent");
        let bad_sym = vm.sym("bad");
        vm.register_state(FixtureState {
            fixture_ty,
            error_ty,
            slot_ty,
            bump_sym,
            receiver_sym,
            counter_sym,
            prop_sym,
            write_only_sym,
            with_slot_sym,
            make_sym,
            meta_sym,
            readonly_sym,
            absent_sym,
            bad_sym,
        });
    }

    fn with_fixture_vm<const N: usize, R: 'static>(
        body: impl for<'v, 's, 'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R + 'static,
    ) -> R {
        with_builder(async move |vm| {
            configure(vm);
            vm.enter_with_slots::<N, _>(body).await
        })
    }

    fn make_fixture<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
        let ty = strand.vm().state::<FixtureState>().fixture_ty;
        ty.create(strand, Fixture, out);
    }

    fn make_slot_fixture<'v>(
        strand: &mut Strand<'v, '_>,
        counter: i64,
        tag: &'static str,
        out: impl Output<'v>,
    ) {
        let ty = strand.vm().state::<FixtureState>().slot_ty;
        ty.create_with_annex(strand, SlotFixture { counter }, SlotAnnex { tag }, out);
    }

    // ── `Unpack`/`UnpackIter`/`UnpackItem` ─────────────────────────────────

    #[test]
    fn unpack_iter_yields_required_optional_key_and_rest_items_in_order() {
        with_fixture_vm(async |strand, []| {
            let const_key = Value::from_i64(strand, 99);
            let opt_default = Value::from_i64(strand, 7);
            let key_default = Value::from_i64(strand, 8);
            let sym_key = strand.vm().state::<FixtureState>().counter_sym;
            let sig = sig::Unpack::new(
                1,
                vec![opt_default],
                vec![
                    sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Sym(sym_key),
                        default: None,
                    },
                    sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Const(const_key),
                        default: Some(key_default),
                    },
                ],
                Variadic::Capture,
            );
            strand
                .with_slots_dynamic(5, async |_strand, slots| {
                    let mut unpack = Unpack { inner: &sig, slots };
                    assert_eq!(unpack.required(), 1);
                    assert_eq!(unpack.optional(), 1);
                    assert_eq!(unpack.required_keys(), 1);
                    assert_eq!(unpack.optional_keys(), 1);
                    assert!(!unpack.exhaustive());
                    assert!(unpack.rest());
                    assert!(unpack.first_required_key().is_some());

                    let items: Vec<_> = unpack.iter().collect();
                    assert_eq!(items.len(), 5);
                    assert!(matches!(&items[0], UnpackItem::Pos { default: None, .. }));
                    assert!(matches!(
                        &items[1],
                        UnpackItem::Pos {
                            default: Some(_),
                            ..
                        }
                    ));
                    assert!(matches!(
                        &items[2],
                        UnpackItem::SymKey { default: None, .. }
                    ));
                    assert!(matches!(
                        &items[3],
                        UnpackItem::ConstKey {
                            default: Some(_),
                            ..
                        }
                    ));
                    assert!(matches!(&items[4], UnpackItem::Rest { .. }));
                })
                .await;
        });
    }

    #[test]
    fn unpack_iter_exhaustive_without_variadic_has_no_rest_item() {
        with_fixture_vm(async |strand, []| {
            let sig = sig::Unpack::new(0, vec![], vec![], Variadic::NONE);
            strand
                .with_slots_dynamic(0, async |_strand, slots| {
                    let mut unpack = Unpack { inner: &sig, slots };
                    assert!(unpack.exhaustive());
                    assert!(!unpack.rest());
                    assert_eq!(unpack.required_keys(), 0);
                    assert_eq!(unpack.optional_keys(), 0);
                    assert!(unpack.first_required_key().is_none());
                    assert!(unpack.iter().next().is_none());
                })
                .await;
        });
    }

    // ── `Object` defaults via `ObjectWrap`'s `Protocol` dispatch ──────────

    #[test]
    fn op_display_debug_verbatim_bool_use_defaults() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut debug = String::new();
                    ObjectWrap::<Fixture>::op_debug(recv.clone(), strand, &mut debug).unwrap();
                    assert_eq!(debug, "<test.Fixture>");

                    let mut display = String::new();
                    ObjectWrap::<Fixture>::op_display(recv.clone(), strand, &mut display).unwrap();
                    assert_eq!(display, "<test.Fixture>");

                    let mut verbatim = String::new();
                    ObjectWrap::<Fixture>::op_verbatim(recv.clone(), strand, &mut verbatim)
                        .unwrap();
                    assert_eq!(verbatim, "<test.Fixture>");

                    assert!(ObjectWrap::<Fixture>::op_bool(recv, strand));
                });
        });
    }

    #[test]
    fn op_call_and_type_object_op_call_use_default_errors() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            let err = ObjectWrap::<Fixture>::op_call(
                                recv,
                                strand,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap_err();
                            assert_eq!(err.kind(), ErrorKind::Type);
                        })
                        .await;
                })
                .await;

            let mut ty_slot = Value::NIL;
            Output::set(strand, Slot::new(&mut ty_slot), state.fixture_ty);
            let ty_value: &Value = &ty_slot;
            state
                .fixture_ty
                .type_vtbl
                .cast(ty_value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            let err = TypeObjectWrap::<Fixture>::op_call(
                                recv,
                                strand,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap_err();
                            assert_eq!(err.kind(), ErrorKind::Type);
                        })
                        .await;
                })
                .await;
        });
    }

    #[test]
    fn op_index_and_op_assign_use_default_type_error() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let idx = Value::from_i64(strand, 0);
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let err = ObjectWrap::<Fixture>::op_index(
                        recv.clone(),
                        strand,
                        &idx,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);

                    let mut idx_slot = idx.dup();
                    let mut val_slot = Value::from_i64(strand, 1);
                    let err = ObjectWrap::<Fixture>::op_assign(
                        recv,
                        strand,
                        Slot::new(&mut idx_slot),
                        Slot::new(&mut val_slot),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);
                });
        });
    }

    #[test]
    fn op_iter_next_sink_put_use_default_type_error() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    let err = ObjectWrap::<Fixture>::op_iter(
                        recv.clone(),
                        strand,
                        Slot::reborrow(&mut out),
                    )
                    .await
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);

                    let err = ObjectWrap::<Fixture>::op_next(
                        recv.clone(),
                        strand,
                        Slot::reborrow(&mut out),
                    )
                    .await
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);

                    let err = ObjectWrap::<Fixture>::op_sink(
                        recv.clone(),
                        strand,
                        Slot::reborrow(&mut out),
                    )
                    .await
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);

                    let mut val = Value::from_i64(strand, 1);
                    let err = ObjectWrap::<Fixture>::op_put(recv, strand, Slot::new(&mut val))
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);
                })
                .await;
        });
    }

    #[test]
    fn op_unpack_empty_fixture_and_op_spread_use_defaults() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            let sig = sig::Unpack::new(0, vec![], vec![], Variadic::NONE);
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, slots| {
                            ObjectWrap::<Fixture>::op_unpack(recv.clone(), strand, &sig, slots)
                                .await
                                .unwrap();
                        })
                        .await;

                    struct NullSpread;
                    impl<'v, 's> protocol::Spread<'v, 's> for NullSpread {
                        fn positional(
                            &mut self,
                            _strand: &mut Strand<'v, 's>,
                            _value: Slot<'v, '_>,
                        ) -> Result<'v, 's, ()> {
                            Ok(())
                        }
                        fn symbol(
                            &mut self,
                            _strand: &mut Strand<'v, 's>,
                            _key: Sym<'v, '_>,
                            _value: Slot<'v, '_>,
                        ) -> Result<'v, 's, ()> {
                            Ok(())
                        }
                        fn keyed(
                            &mut self,
                            _strand: &mut Strand<'v, 's>,
                            _key: Slot<'v, '_>,
                            _value: Slot<'v, '_>,
                        ) -> Result<'v, 's, ()> {
                            Ok(())
                        }
                    }
                    let mut sink = NullSpread;
                    // The default `spread` returns `Unsupported`, which `op_spread` catches
                    // and retries via the generic `default_spread` adapter based on
                    // iteration; since `Fixture` also doesn't support iteration, that
                    // fails too, but with a `Type` error surfaced from `op_iter`.
                    let err = ObjectWrap::<Fixture>::op_spread(
                        recv,
                        strand,
                        protocol::SpreadContext::Sequence,
                        &mut sink,
                    )
                    .await
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Type);
                })
                .await;
            let _ = &mut out;
        });
    }

    #[test]
    fn default_native_unpack_reads_registered_fields_and_captures_lazy_tail() {
        with_fixture_vm(async |strand, [mut owner, mut tail, mut tmp]| {
            make_slot_fixture(strand, 4, "unpack", Slot::reborrow(&mut owner));
            let state = strand.vm().state::<FixtureState>();
            let sig = sig::Unpack::new(
                0,
                vec![],
                vec![sig::UnpackKey {
                    kind: sig::UnpackKeyKind::Sym(state.counter_sym),
                    default: None,
                }],
                Variadic::Capture,
            );
            strand
                .with_slots_dynamic(2, async |strand, mut out| {
                    owner
                        .op_unpack(strand, &sig, unsafe { Slots::new(out.as_inner()) })
                        .await
                        .unwrap();
                    assert_eq!(out.at(0).to_i64(strand).unwrap(), 4);
                    tail.store(out.at(1).take());
                })
                .await;

            owner
                .op_get(strand, state.counter_sym, Slot::reborrow(&mut tmp))
                .unwrap();
            assert_eq!(tmp.to_i64(strand).unwrap(), 4, "capture evaluated a getter");

            let absent_sig = sig::Unpack::new(
                0,
                vec![],
                vec![sig::UnpackKey {
                    kind: sig::UnpackKeyKind::Sym(state.absent_sym),
                    default: Some(Value::from_i64(strand, 17)),
                }],
                Variadic::Capture,
            );
            strand
                .with_slots_dynamic(2, async |strand, mut out| {
                    tail.op_unpack(strand, &absent_sig, unsafe { Slots::new(out.as_inner()) })
                        .await
                        .unwrap();
                    assert_eq!(out.at(0).to_i64(strand).unwrap(), 17);
                    // Recursive capture returns the same stateful iterator.
                    assert!(out.at(1).repr_eq(strand, &tail));
                })
                .await;
            owner
                .op_get(strand, state.counter_sym, Slot::reborrow(&mut tmp))
                .unwrap();
            assert_eq!(tmp.to_i64(strand).unwrap(), 5);

            let bad_sig = sig::Unpack::new(
                0,
                vec![],
                vec![sig::UnpackKey {
                    kind: sig::UnpackKeyKind::Sym(state.bad_sym),
                    default: None,
                }],
                Variadic::Capture,
            );
            strand
                .with_slots_dynamic(2, async |strand, mut out| {
                    out.at(0).store(Value::from_i64(strand, 99));
                    let err = tail
                        .op_unpack(strand, &bad_sig, unsafe { Slots::new(out.as_inner()) })
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Value);
                    assert_eq!(out.at(0).to_i64(strand).unwrap(), 99);
                })
                .await;
        });
    }

    #[test]
    fn default_native_exhaustive_unpack_evaluates_unmatched_getters_atomically() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_slot_fixture(strand, 4, "unpack", Slot::reborrow(&mut owner));
            let state = strand.vm().state::<FixtureState>();
            let sig = sig::Unpack::new(
                0,
                vec![],
                vec![
                    sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Sym(state.counter_sym),
                        default: None,
                    },
                    sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Sym(state.prop_sym),
                        default: None,
                    },
                ],
                Variadic::NONE,
            );
            strand
                .with_slots_dynamic(2, async |strand, mut out| {
                    out.at(0).store(Value::from_i64(strand, 90));
                    out.at(1).store(Value::from_i64(strand, 91));
                    let err = owner
                        .op_unpack(strand, &sig, unsafe { Slots::new(out.as_inner()) })
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Value);
                    assert_eq!(out.at(0).to_i64(strand).unwrap(), 90);
                    assert_eq!(out.at(1).to_i64(strand).unwrap(), 91);
                })
                .await;

            // The absent getter was checked before the failing getter.
            strand.with_slots_sync(|strand, [mut tmp]| {
                owner
                    .op_get(strand, state.counter_sym, Slot::reborrow(&mut tmp))
                    .unwrap();
                assert_eq!(tmp.to_i64(strand).unwrap(), 5);
            });
        });
    }

    #[test]
    fn op_hash_is_stable_and_op_eq_ne_use_default_type_error() {
        with_fixture_vm(async |strand, [mut owner, mut other]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            make_fixture(strand, Slot::reborrow(&mut other));
            let value: &Value = &owner;
            let other_value: &Value = &other;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    use std::{collections::hash_map::DefaultHasher, hash::Hasher};
                    let mut h1 = DefaultHasher::new();
                    ObjectWrap::<Fixture>::op_hash(recv.clone(), strand, &mut h1).unwrap();
                    let mut h2 = DefaultHasher::new();
                    ObjectWrap::<Fixture>::op_hash(recv.clone(), strand, &mut h2).unwrap();
                    assert_eq!(h1.finish(), h2.finish());

                    match ObjectWrap::<Fixture>::op_eq(recv.clone(), strand, other_value) {
                        Err(err) => assert_eq!(err.kind(), ErrorKind::Type),
                        Ok(_) => panic!("expected a type error"),
                    }
                    match ObjectWrap::<Fixture>::op_ne(recv, strand, other_value) {
                        Err(err) => assert_eq!(err.kind(), ErrorKind::Type),
                        Ok(_) => panic!("expected a type error"),
                    }
                });
        });
    }

    #[test]
    fn op_arithmetic_and_comparison_use_default_type_error() {
        with_fixture_vm(async |strand, [mut owner, mut other]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            Output::set(strand, Slot::reborrow(&mut other), 1_i64);
            let value: &Value = &owner;
            let other_value: &Value = &other;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    macro_rules! assert_type_err {
                        ($e:expr) => {
                            match $e {
                                Err(err) => assert_eq!(err.kind(), ErrorKind::Type),
                                Ok(_) => panic!("expected a type error"),
                            }
                        };
                    }
                    assert_type_err!(ObjectWrap::<Fixture>::op_neg(recv.clone(), strand));
                    assert_type_err!(ObjectWrap::<Fixture>::op_bnot(recv.clone(), strand));
                    assert_type_err!(ObjectWrap::<Fixture>::op_band(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_bor(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_bxor(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_shl(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_shr(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_add(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_sub(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_rsub(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_mul(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_div(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_rdiv(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_ediv(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_rediv(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_mod(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_rmod(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_lt(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_lte(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_gt(
                        recv.clone(),
                        strand,
                        other_value
                    ));
                    assert_type_err!(ObjectWrap::<Fixture>::op_gte(recv, strand, other_value));
                });
        });
    }

    #[test]
    fn delegated_op_mcall_falls_back_to_method_with_delegator_when_no_entry_matches() {
        with_fixture_vm(async |strand, [mut owner, mut delegator, mut out]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            Output::set(strand, Slot::reborrow(&mut delegator), 42_i64);
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<Fixture>::op_mcall(
                                recv.with_delegator(&delegator),
                                strand,
                                Sym::well_known(sym::LEN),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                            assert!(out.repr_eq(strand, &delegator));
                        })
                        .await;
                })
                .await;
        });
    }

    #[test]
    fn delegated_registered_handler_receives_semantic_receiver() {
        with_fixture_vm(async |strand, [mut owner, mut delegator, mut out]| {
            make_slot_fixture(strand, 0, "x", Slot::reborrow(&mut owner));
            Output::set(strand, Slot::reborrow(&mut delegator), 42_i64);
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv.with_delegator(&delegator),
                                strand,
                                state.receiver_sym,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                            assert!(out.repr_eq(strand, &delegator));
                        })
                        .await;
                })
                .await;
            assert!(out.repr_eq(strand, &delegator));
        });
    }

    // ── Registered-entry dispatch on `SlotFixture` instances ──────────────

    #[test]
    fn op_get_dispatches_getter_property_method_and_default_field() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_slot_fixture(strand, 3, "x", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    ObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        state.counter_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                    assert_eq!(out.to_i64(strand).unwrap(), 3);

                    let prop_sym = state.prop_sym;
                    ObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        prop_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                    assert_eq!(out.to_i64(strand).unwrap(), 11);

                    ObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        state.bump_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();

                    let write_only_sym = state.write_only_sym;
                    let err = ObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        write_only_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);

                    let err = ObjectWrap::<SlotFixture>::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });
        });
    }

    #[test]
    fn op_set_dispatches_setter_property_and_default_immutable_field() {
        with_fixture_vm(async |strand, [mut owner, mut val]| {
            make_slot_fixture(strand, 0, "x", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Output::set(strand, Slot::reborrow(&mut val), 1_i64);
                    let prop_sym = state.prop_sym;
                    ObjectWrap::<SlotFixture>::op_set(
                        recv.clone(),
                        strand,
                        prop_sym,
                        Slot::reborrow(&mut val),
                    )
                    .unwrap();

                    let write_only_sym = state.write_only_sym;
                    ObjectWrap::<SlotFixture>::op_set(
                        recv.clone(),
                        strand,
                        write_only_sym,
                        Slot::reborrow(&mut val),
                    )
                    .unwrap();

                    let err = ObjectWrap::<SlotFixture>::op_set(
                        recv.clone(),
                        strand,
                        state.counter_sym,
                        Slot::reborrow(&mut val),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Immutable);

                    let err = ObjectWrap::<SlotFixture>::op_set(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut val),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });
        });
    }

    #[test]
    fn op_mcall_dispatches_method_synthetic_get_set_and_default_field() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_slot_fixture(strand, 0, "x", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // Registered method entry.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                state.bump_sym,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                    assert_eq!(out.to_i64(strand).unwrap(), 1);

                    // Synthetic `(get)` special-cased inside `op_mcall`.
                    strand
                        .with_slots_dynamic(1, async |strand, mut arg_slots| {
                            Output::set(strand, arg_slots.at(0), state.counter_sym);
                            let sig = [None];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                Sym::well_known(sym::GET_METHOD),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                    assert_eq!(out.to_i64(strand).unwrap(), 1);

                    // Synthetic `(set)` special-cased inside `op_mcall`.
                    strand
                        .with_slots_dynamic(2, async |strand, mut arg_slots| {
                            let prop_sym = state.prop_sym;
                            Output::set(strand, arg_slots.at(0), prop_sym);
                            Output::set(strand, arg_slots.at(1), 42_i64);
                            let sig = [None, None];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                Sym::well_known(sym::SET_METHOD),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;

                    // Unregistered special methods use the receiver protocol.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                Sym::well_known(sym::BOOL_METHOD),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                    assert!(out.to_bool(strand));

                    // Falls through to `T::method`'s default field error.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            let err = ObjectWrap::<SlotFixture>::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::LEN),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap_err();
                            assert_eq!(err.kind(), ErrorKind::Field);
                        })
                        .await;
                })
                .await;
        });
    }

    // ── `TypeObjectWrap` dispatch (type-object singleton) ──────────────────

    fn type_value<'v, T: Object<'v>>(
        strand: &mut Strand<'v, '_>,
        ty: Type<'v, T>,
        out: impl Output<'v>,
    ) {
        Output::set(strand, out, ty);
    }

    #[test]
    fn type_object_inspect_borrows_stable_semantic_members() {
        with_fixture_vm(async |strand, [mut ty_slot]| {
            let state = strand.vm().state::<FixtureState>();
            type_value(strand, state.slot_ty, Slot::reborrow(&mut ty_slot));
            let ty_value: &Value = &ty_slot;

            state
                .slot_ty
                .type_vtbl
                .cast(ty_value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let inspect =
                        TypeObjectWrap::<SlotFixture>::op_inspect(recv.clone(), strand.vm())
                            .unwrap();
                    let members = inspect.members;
                    let kind = |sym| {
                        members
                            .iter()
                            .find(|member| member.sym == sym)
                            .map(|member| member.kind)
                    };

                    assert_eq!(kind(state.bump_sym), Some(MemberKind::Method));
                    assert_eq!(kind(state.counter_sym), Some(MemberKind::Getter));
                    assert_eq!(kind(state.prop_sym), Some(MemberKind::Property));
                    assert_eq!(kind(state.write_only_sym), Some(MemberKind::Setter));

                    let type_members = inspect.type_members;
                    let type_kind = |sym| {
                        type_members
                            .iter()
                            .find(|member| member.sym == sym)
                            .map(|member| member.kind)
                    };
                    assert_eq!(type_kind(state.make_sym), Some(MemberKind::Method));
                    assert_eq!(type_kind(state.meta_sym), Some(MemberKind::Property));
                    assert_eq!(type_kind(state.readonly_sym), Some(MemberKind::Getter));
                    assert_eq!(type_kind(state.bump_sym), None);

                    let again =
                        TypeObjectWrap::<SlotFixture>::op_inspect(recv, strand.vm()).unwrap();
                    assert!(std::ptr::eq(members, again.members));
                    assert!(std::ptr::eq(type_members, again.type_members));
                });
        });
    }

    #[test]
    fn type_object_op_mcall_type_method_unbound_method_and_default_field() {
        with_fixture_vm(async |strand, [mut ty_slot, mut inst_slot, mut out]| {
            let state = strand.vm().state::<FixtureState>();
            type_value(strand, state.slot_ty, Slot::reborrow(&mut ty_slot));
            make_slot_fixture(strand, 4, "u", Slot::reborrow(&mut inst_slot));
            let ty_value: &Value = &ty_slot;
            let inst_value = inst_slot.dup();

            state
                .slot_ty
                .type_vtbl
                .cast(ty_value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // Registered type-method entry: instantiates a fresh object.
                    let make_sym = state.make_sym;
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            TypeObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                make_sym,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                    assert!(state.slot_ty.cast(&out).is_some());

                    // Unbound instance-method call: `SlotFixture.bump(instance)`.
                    strand
                        .with_slots_dynamic(1, async |strand, mut arg_slots| {
                            Output::set(strand, arg_slots.at(0), &inst_value);
                            let sig = [None];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            TypeObjectWrap::<SlotFixture>::op_mcall(
                                recv.clone(),
                                strand,
                                state.bump_sym,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                    assert_eq!(out.to_i64(strand).unwrap(), 5);

                    // Falls through to `T::type_method`'s default field error.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            let err = TypeObjectWrap::<SlotFixture>::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::LEN),
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap_err();
                            assert_eq!(err.kind(), ErrorKind::Field);
                        })
                        .await;
                })
                .await;
        });
    }

    #[test]
    fn type_object_op_get_op_set_dispatch() {
        with_fixture_vm(async |strand, [mut ty_slot, mut out]| {
            let state = strand.vm().state::<FixtureState>();
            type_value(strand, state.slot_ty, Slot::reborrow(&mut ty_slot));
            let ty_value: &Value = &ty_slot;

            state
                .slot_ty
                .type_vtbl
                .cast(ty_value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let meta_sym = state.meta_sym;
                    // Registered type-level getter.
                    TypeObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        meta_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                    assert_eq!(out.to_i64(strand).unwrap(), 0);

                    // Registered type-level setter, then read back through the getter.
                    let mut val = Value::from_i64(strand, 7);
                    TypeObjectWrap::<SlotFixture>::op_set(
                        recv.clone(),
                        strand,
                        meta_sym,
                        Slot::new(&mut val),
                    )
                    .unwrap();
                    TypeObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        meta_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                    assert_eq!(out.to_i64(strand).unwrap(), 7);

                    // Unbound instance entries: bound-method creation via `op_get`.
                    TypeObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        state.bump_sym,
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();

                    // `GET_METHOD`/protocol syms are always resolvable to a bound method.
                    TypeObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        Sym::well_known(sym::GET_METHOD),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();

                    // Getter-only type-level entry: setting it through the type object errors.
                    let mut val2 = Value::from_i64(strand, 1);
                    let err = TypeObjectWrap::<SlotFixture>::op_set(
                        recv.clone(),
                        strand,
                        state.readonly_sym,
                        Slot::new(&mut val2),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Immutable);

                    // Falls through to `T::type_get`/`T::type_set` default field errors.
                    let err = TypeObjectWrap::<SlotFixture>::op_get(
                        recv.clone(),
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);

                    let mut val3 = Value::from_i64(strand, 1);
                    let err = TypeObjectWrap::<SlotFixture>::op_set(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::new(&mut val3),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });
        });
    }

    #[test]
    fn type_object_op_hash_op_eq_op_subtype() {
        with_fixture_vm(async |strand, [mut ty_slot, mut other_ty_slot]| {
            let state = strand.vm().state::<FixtureState>();
            type_value(strand, state.slot_ty, Slot::reborrow(&mut ty_slot));
            type_value(strand, state.fixture_ty, Slot::reborrow(&mut other_ty_slot));
            let ty_value: &Value = &ty_slot;
            let other_ty_value: &Value = &other_ty_slot;

            state
                .slot_ty
                .type_vtbl
                .cast(ty_value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    use std::{collections::hash_map::DefaultHasher, hash::Hasher};
                    let mut h1 = DefaultHasher::new();
                    TypeObjectWrap::<SlotFixture>::op_hash(recv.clone(), strand, &mut h1).unwrap();
                    let mut h2 = DefaultHasher::new();
                    TypeObjectWrap::<SlotFixture>::op_hash(recv.clone(), strand, &mut h2).unwrap();
                    assert_eq!(h1.finish(), h2.finish());

                    let eq = TypeObjectWrap::<SlotFixture>::op_eq(recv.clone(), strand, ty_value)
                        .unwrap();
                    assert!(eq.to_bool(strand));
                    let ne =
                        TypeObjectWrap::<SlotFixture>::op_eq(recv.clone(), strand, other_ty_value)
                            .unwrap();
                    assert!(!ne.to_bool(strand));

                    assert!(TypeObjectWrap::<SlotFixture>::op_subtype(
                        recv.clone(),
                        strand,
                        ty_value
                    ));
                    let universal = Value::from_input(strand, TypeObject::Value);
                    assert!(TypeObjectWrap::<SlotFixture>::op_subtype(
                        recv.clone(),
                        strand,
                        &universal
                    ));
                    assert!(!TypeObjectWrap::<SlotFixture>::op_subtype(
                        recv,
                        strand,
                        other_ty_value
                    ));
                });
        });
    }

    // ── `Instance`/`Ref`/`Mut`/`Annex`/`Cast`/`Type` accessors ─────────────

    #[test]
    fn instance_borrow_borrow_mut_and_annex_access() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_slot_fixture(strand, 3, "hello", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, instance| {
                    assert_eq!(instance.borrow(strand).unwrap().counter, 3);
                    assert_eq!(instance.borrow_unwrap().counter, 3);
                    {
                        let mut m = instance.borrow_mut(strand).unwrap();
                        m.counter = 9;
                    }
                    assert_eq!(instance.borrow_mut_unwrap().counter, 9);
                    assert_eq!(instance.annex().tag, "hello");
                });
        });
    }

    #[test]
    fn instance_slot_read_write_roundtrip() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_slot_fixture(strand, 0, "x", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .slot_ty
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, instance| {
                    {
                        let mut m = instance.borrow_mut_unwrap();
                        Output::set(strand, Mut::slot_mut::<0>(&mut m), 77_i64);
                    }
                    let r = instance.borrow_unwrap();
                    assert_eq!(Ref::slot::<0>(&r).to_i64(strand).unwrap(), 77);
                    drop(r);
                    let m = instance.borrow_mut_unwrap();
                    assert_eq!(Mut::slot::<0>(&m).to_i64(strand).unwrap(), 77);
                });
        });
    }

    #[test]
    fn instance_ty_reconstructs_a_type_handle_that_still_casts_the_value() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, instance| {
                    let ty = instance.ty(strand.vm());
                    assert!(ty.cast(value).is_some());
                });
        });
    }

    #[test]
    fn cast_enter_finalize_runs_without_a_live_strand() {
        with_fixture_vm(async |strand, [mut owner]| {
            make_fixture(strand, Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            let cast = state.fixture_ty.cast(value).unwrap();
            let bool_result = cast.enter_finalize(|instance| Fixture::bool(instance, strand));
            assert!(bool_result);
        });
    }

    #[test]
    fn type_borrow_and_annex_roundtrip() {
        with_fixture_vm(async |strand, []| {
            let state = strand.vm().state::<FixtureState>();
            assert_eq!(*state.slot_ty.borrow(strand).unwrap(), 0);
            {
                let mut m = state.slot_ty.borrow_mut(strand).unwrap();
                *m = 5;
            }
            assert_eq!(*state.slot_ty.borrow_unwrap(strand.vm()), 5);
            {
                let mut m = state.slot_ty.borrow_mut_unwrap(strand.vm());
                *m = 6;
            }
            assert_eq!(*state.slot_ty.borrow(strand).unwrap(), 6);
            assert_eq!(*state.slot_ty.annex(strand.vm()), 0);
        });
    }

    #[test]
    fn error_kind_follows_the_declared_nominal_supertype() {
        with_fixture_vm(async |strand, [mut declared, mut plain]| {
            let state = strand.vm().state::<FixtureState>();
            state
                .error_ty
                .create(strand, ErrorFixture, Slot::reborrow(&mut declared));
            state
                .fixture_ty
                .create(strand, Fixture, Slot::reborrow(&mut plain));

            let err = Error::from_value(strand, &declared);
            assert_eq!(err.kind(), ErrorKind::Value);
            assert!(err.catchable());

            // A type that declares no error supertype is still just a runtime error.
            let err = Error::from_value(strand, &plain);
            assert_eq!(err.kind(), ErrorKind::Runtime);
        });
    }

    #[test]
    fn type_create_and_create_with_annex_produce_castable_instances() {
        with_fixture_vm(async |strand, [mut out1, mut out2]| {
            let state = strand.vm().state::<FixtureState>();
            state
                .fixture_ty
                .create(strand, Fixture, Slot::reborrow(&mut out1));
            assert!(state.fixture_ty.cast(&out1).is_some());

            state.slot_ty.create_with_annex(
                strand,
                SlotFixture { counter: 1 },
                SlotAnnex { tag: "y" },
                Slot::reborrow(&mut out2),
            );
            assert!(state.slot_ty.cast(&out2).is_some());
            assert!(state.fixture_ty.cast(&out2).is_none());
        });
    }

    #[test]
    fn method_with_slots_dispatch_uses_scratch_slot() {
        with_fixture_vm(async |strand, [mut owner, mut out]| {
            make_slot_fixture(strand, 0, "x", Slot::reborrow(&mut owner));
            let value: &Value = &owner;
            let state = strand.vm().state::<FixtureState>();
            let with_slot_sym = state.with_slot_sym;
            state
                .slot_ty
                .vtbl
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            ObjectWrap::<SlotFixture>::op_mcall(
                                recv,
                                strand,
                                with_slot_sym,
                                args,
                                Slot::reborrow(&mut out),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                })
                .await;
            assert_eq!(out.to_i64(strand).unwrap(), 5);
        });
    }
}
