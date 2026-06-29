use std::{
    borrow::Cow,
    cell::UnsafeCell,
    fmt, future,
    hash::Hasher,
    marker::PhantomData,
    ops::{ControlFlow, Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt},
    gc::{
        self, Base, Collect,
        arena::{self, Upcast, Visit},
    },
    object::protocol::{GcObjBorrow, Recv, Vtbl},
    sig,
    strand::{Pinned, Strand},
    sym::{self, Sym},
    unpack,
    value::{Case, Input, InputBy, Output, Slot, Slots, TypeObject, Value, private::Sealed},
    vm::{Alloc, Builder, Vm},
};

use super::{
    BoundMethod,
    protocol::{self, Inspect, Protocol, TypeHandle, dispatch_native_method},
};
use dolang_bytecode::Variadic;
use dolang_util::alias;

pub use super::protocol::{Spread, SpreadContext};

pub(crate) struct ObjectWrap<'v, T>(T, PhantomData<&'v mut &'v ()>);

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
        self.inner.variadic == Variadic::None
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
            let key = &unpack.inner.keys[i];
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
    /// When this call arrived via class delegation (`op_dcall`), the original
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
        &self.0.0
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
        &self.0.0
    }
}

impl<'v, 'a, T: Object<'v>> DerefMut for Mut<'v, 'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.0
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
    pub(crate) fn new(receiver: gc::Borrow<'v, 'a, protocol::Header, ObjectWrap<'v, T>>) -> Self {
        Self {
            receiver,
            delegator: None,
        }
    }

    /// Borrow content immutably
    #[inline]
    pub fn borrow<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Ref<'v, 'a, T>> {
        Ok(Ref(self
            .receiver
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?))
    }

    /// Borrow content immutably, panicking on failure
    #[inline]
    pub fn borrow_unwrap(&self) -> Ref<'v, 'a, T> {
        Ref(self.receiver.borrow().expect("conflicting borrow"))
    }

    /// Borrow content mutably
    #[inline]
    pub fn borrow_mut<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Mut<'v, 'a, T>> {
        Ok(Mut(self
            .receiver
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?))
    }

    /// Borrow content mutably, panicking on failure
    #[inline]
    pub fn borrow_mut_unwrap(&self) -> Mut<'v, 'a, T> {
        Mut(self.receiver.borrow_mut().expect("conflicting borrow"))
    }

    /// Get immutable annex
    #[inline]
    pub fn annex(&self) -> Annex<'v, 'a, T> {
        Annex(&self.receiver.annex().inner)
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

    /// Implements string conversion for external program arguments by writing a string
    /// representation to `w`.
    fn display_arg<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::display(this, strand, w)
    }

    /// Implements canonical string conversion by writing a string representation to `w`.
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::debug(this, strand, w)
    }

    /// Implements debug string conversion by writing a string representation to `w`.
    /// Compared to [`Object::display`], which is intended to be canonical and user-readable,
    /// debug string forms are intended for developer consumption.
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let _ = this;
        write!(w, "<{}.{}>", Self::MODULE, Self::NAME).into_do(strand)
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
    /// input iterator object which should implement [`Object::next`]
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn input<'a, 's>(
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
            format!("input iterator `next` not supported: {}", Self::NAME),
        )))
    }

    /// Implements Do output iteration (`output`), setting `out` to an
    /// output iterator object which should implement [`Object::put`]
    /// # Default
    /// Returns a type error
    #[allow(unused_variables)]
    fn output<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::type_error(
            strand,
            format!("output iteration not supported: {}", Self::NAME),
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
            format!("output iterator `put` not supported: {}", Self::NAME),
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
    /// Returns [`Error::not_supported`], indicating the object does not support unpacking.
    ///
    /// # Atomicity
    ///
    /// When practical, implementations should make unpacking **atomic**: if the operation
    /// fails partway through, the object's observable state should remain unchanged.
    /// See [`Unpack`] documentation for patterns and examples.
    #[allow(unused_variables)]
    fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> impl Future<Output = Result<'v, 's, ()>> {
        future::ready(Err(Error::not_supported(strand)))
    }

    /// Performs any cleanup before the objects slots are cleared during garbage collection.
    ///
    /// This method is called synchronously during GC when the object is being collected.
    /// It allows objects to perform cleanup that requires access to their slots.
    /// before the slots are cleared to `nil`.
    ///
    /// # Default
    /// Does nothing.
    #[allow(unused_variables)]
    fn clear<'a>(this: Instance<'v, 'a, Self>) {}
}

unsafe impl<'v, T: Object<'v>> Collect for ObjectWrap<'v, T> {
    const CYCLIC: bool = T::SLOTS != 0;
    const IMMUTABLE: bool = false;
    type Annex = ObjectAnnex<'v, T>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn pre_clear(this: NonNull<arena::Header>) {
        if T::SLOTS != 0 {
            unsafe {
                // Call the user Object::clear hook
                T::clear(Instance::new(gc::Borrow::new(this.cast())));
            }
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
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(str)")),
            |strand| T::display(Instance::new(this.receiver), strand, w),
        )
    }

    fn op_display_arg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(arg)")),
            |strand| T::display_arg(Instance::new(this.receiver), strand, w),
        )
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Strand::for_native_frame(
            strand,
            Cow::Borrowed(T::MODULE),
            Cow::Borrowed(T::NAME),
            Some(Cow::Borrowed("(dbg)")),
            |strand| T::debug(Instance::new(this.receiver), strand, w),
        )
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
            async |strand| T::call(Instance::new(this.receiver), strand, args, out).await,
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
                            .call(this.as_header(), None, strand, args, out)
                            .await
                    },
                    Entry::Delegate(idx) => {
                        let supertype =
                            instance_supertype::<T>(Recv::new(this.receiver), strand, *idx);
                        strand
                            .with_slots(async |strand, [mut delegator]| {
                                delegator.store(Value::from_object(Base::upcast(
                                    this.receiver.to_strong(),
                                )));
                                supertype
                                    .op_dcall(strand, &delegator, method, args, out)
                                    .await
                            })
                            .await
                    }
                    _ => Err(Error::field(strand, method)),
                },
            )
            .await
        } else {
            T::method(Instance::new(this.receiver), strand, method, args, out).await
        }
    }

    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
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
                            .call(this.as_header(), Some(delegator), strand, args, out)
                            .await
                    },
                    Entry::Delegate(idx) => {
                        let supertype =
                            instance_supertype::<T>(Recv::new(this.receiver), strand, *idx);
                        supertype
                            .op_dcall(strand, delegator, method, args, out)
                            .await
                    }
                    _ => Err(Error::field(strand, method)),
                },
            )
            .await
        } else {
            T::method(
                Instance {
                    receiver: this.receiver,
                    delegator: Some(delegator),
                },
                strand,
                method,
                args,
                out,
            )
            .await
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
                    handler.call(this.as_header(), strand, out)
                },
                Some(Entry::Method(_) | Entry::Delegate(_)) => {
                    BoundMethod::create(strand, &this, field, out);
                    Ok(())
                }
                Some(Entry::Setter(_)) => Err(Error::field(strand, field)),
                None => T::get(Instance::new(this.receiver), strand, field, out),
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
                    handler.call(this.as_header(), strand, value)
                },
                _ => T::set(Instance::new(this.receiver), strand, field, value),
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
            |strand| T::index(Instance::new(this.receiver), strand, index, out),
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
            |strand| T::assign(Instance::new(this.receiver), strand, index, value),
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
            async |strand| T::input(Instance::new(this.receiver), strand, out).await,
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
            async |strand| T::next(Instance::new(this.receiver), strand, out).await,
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
            async |strand| T::output(Instance::new(this.receiver), strand, out).await,
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
            async |strand| T::put(Instance::new(this.receiver), strand, value).await,
        )
        .await
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        T::hash(Instance::new(this.receiver), _strand, hasher)
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
            |strand| T::eq(Instance::new(this.receiver), strand, other),
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
            |strand| T::ne(Instance::new(this.receiver), strand, other),
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
            |strand| T::neg(Instance::new(this.receiver), strand, Slot::new(&mut out)),
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
            |strand| T::bnot(Instance::new(this.receiver), strand, Slot::new(&mut out)),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
                    Instance::new(this.receiver),
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
            |strand| T::lt(Instance::new(this.receiver), strand, other),
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
            |strand| T::lte(Instance::new(this.receiver), strand, other),
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
            |strand| T::gt(Instance::new(this.receiver), strand, other),
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
            |strand| T::gte(Instance::new(this.receiver), strand, other),
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
                    Instance::new(this.receiver),
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
            async |strand| T::spread(Instance::new(this.receiver), strand, context, sink).await,
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
        InputBy::Borrow(&vm.type_singletons[self.singleton_idx])
    }
}

impl<'v, T: Object<'v>> Type<'v, T> {
    pub(crate) fn create_raw(&self, vm: &Vm<'v>, value: T, annex: T::Annex) -> Value<'v> {
        Value::from_object(protocol::GcObj::new_annex(
            vm.arena(),
            self.vtbl,
            ObjectWrap(value, PhantomData),
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

    /// Downcast value to [`Instance`]
    pub fn downcast<'a>(&self, value: &'a Value<'v>) -> Option<Instance<'v, 'a, T>> {
        value.downcast_ref(self.vtbl).map(Instance::new)
    }

    /// Reconstructs a `Type<'v,T>` from a raw type object GC header.
    ///
    /// # Safety
    /// `header` must point to a GC-allocated `TypeObjectWrap<'v,T>` whose vtable is
    /// `TypeVtbl<'v>`.
    pub(crate) unsafe fn from_type_header(header: NonNull<arena::Header>) -> Self {
        let vtbl = unsafe { header.as_ref().vtbl().cast::<TypeVtbl<'v>>() };
        Type {
            vtbl: unsafe { TypeHandle::new(vtbl.as_ref().inst_vtbl.cast()) },
            type_vtbl: unsafe { TypeHandle::new(vtbl.cast()) },
            singleton_idx: unsafe { vtbl.as_ref().inst_vtbl.as_ref().singleton_idx },
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
    call: for<'a, 's> unsafe fn(
        closure: NonNull<()>,
        header: NonNull<protocol::Header>,
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
        strand: &'a mut Strand<'v, 's>,
        slot: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unsafe { (self.call)(self.closure, header, strand, slot) }
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
    Delegate(usize),
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
    vm: &'a mut Builder<'v>,
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
    pub(crate) vm: &'a mut Builder<'v>,
    pub(crate) entries: Vec<(Sym<'v, 'v>, Entry<'v>)>,
    pub(crate) type_entries: Vec<(Sym<'v, 'v>, Entry<'v>)>,
    pub(crate) supertypes: Vec<Value<'v>>,
    pub(crate) nominal_supertypes: Vec<Value<'v>>,
}

impl<'v, 'a> Deref for TypeBuilderInner<'v, 'a> {
    type Target = Builder<'v>;
    fn deref(&self) -> &Builder<'v> {
        self.vm
    }
}

impl<'v, 'a> DerefMut for TypeBuilderInner<'v, 'a> {
    fn deref_mut(&mut self) -> &mut Builder<'v> {
        self.vm
    }
}

impl<'v, 'a> TypeBuilderInner<'v, 'a> {
    fn new(vm: &'a mut Builder<'v>) -> Self {
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
            panic!("native object supertype must be abstract");
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
                let member = unsafe { member.into_static_scope_unchecked() };
                // Local entries win; first matching supertype wins.
                if !self.entries.iter().any(|(sym, _)| *sym == member) {
                    self.entries.push((member, Entry::Delegate(supertype_idx)));
                }
            }
        }

        let entries = merge_entries(self.entries);
        let type_entries = merge_entries(self.type_entries);

        let idx = self.vm.inner.type_singletons.len();

        let inst_vtbl = self.vm.inner.types.register(ObjectVtbl {
            base: inst_vtbl_base,
            entries: entries.into(),
            singleton_idx: idx,
        });

        let type_vtbl = self.vm.inner.types.register(TypeVtbl {
            base: type_vtbl_base,
            entries: type_entries.into(),
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
/// Returned by [`Builder::build_type`](crate::vm::Builder::build_type) after passing through
/// [`Object::build`], and consumed by [`TypeBuilder::build`] to create the native type.
/// Implements [`Deref`]/[`DerefMut`] to [`Builder`] so that `Builder` methods
/// (e.g. [`Builder::sym`]) can be called directly when capturing symbols in closures.
pub struct TypeBuilder<'v, 'a, T: Object<'v>> {
    pub(crate) inner: TypeBuilderInner<'v, 'a>,
    type_value: T::Type,
    type_annex: T::TypeAnnex,
    _phantom: PhantomData<T>,
}

impl<'v, 'a, T: Object<'v>> Deref for TypeBuilder<'v, 'a, T> {
    type Target = Builder<'v>;
    fn deref(&self) -> &Builder<'v> {
        self.inner.vm
    }
}

impl<'v, 'a, T: Object<'v>> DerefMut for TypeBuilder<'v, 'a, T> {
    fn deref_mut(&mut self) -> &mut Builder<'v> {
        self.inner.vm
    }
}

impl<'v, 'a, T: Object<'v>> TypeBuilder<'v, 'a, T> {
    pub(crate) fn new(
        vm: &'a mut Builder<'v>,
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
            let prev = std::mem::replace(last_entry, Entry::Delegate(0)); // placeholder
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
    strand: &'a mut Strand<'v, 's>,
    slot: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    T: Object<'v>,
    F: for<'b, 'r> Fn(Instance<'v, 'b, T>, &mut Strand<'v, 'r>, Slot<'v, 'b>) -> Result<'v, 'r, ()>
        + 'v,
{
    let f = unsafe { closure.cast::<F>().as_ref() };
    let inst = Instance::new(unsafe { gc::Borrow::from_raw(header.cast()) });
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
    fn vtbl(&self) -> &TypeVtbl<'v> {
        unsafe { self.vtbl_downcast_unchecked::<TypeVtbl<'v>>() }
    }

    fn inst_vtbl(&self) -> &ObjectVtbl<'v> {
        unsafe { self.vtbl().inst_vtbl.as_ref() }
    }

    fn entry(&self, sym: Sym<'v, '_>) -> Option<&Entry<'v>> {
        self.vtbl().entry(sym)
    }

    fn inst_entry(&self, sym: Sym<'v, '_>) -> Option<&Entry<'v>> {
        self.inst_vtbl().entry(sym)
    }

    /// Returns a reference to the type singleton [`Value`] for this type.
    fn singleton<'b>(&self, vm: &'b Vm<'v>) -> &'b Value<'v> {
        &vm.type_singletons[self.inst_vtbl().singleton_idx]
    }

    fn ty(&self) -> Type<'v, T> {
        // SAFETY: `self` is a live `TypeObjectWrap<'v, T>` receiver.
        unsafe { Type::<T>::from_type_header(self.as_header().cast()) }
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type: {}.{}>", T::MODULE, T::NAME).into_do(strand)
    }

    fn op_inspect<'a>(this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        let mut members = vec![
            Sym::well_known(sym::INIT_METHOD),
            // All protocol/special methods handled by dispatch_native_method.
            // Reported unconditionally since we cannot statically know which
            // the Object impl supports; unsupported ops will error at call time.
            Sym::well_known(sym::STR_METHOD),
            Sym::well_known(sym::DBG_METHOD),
            Sym::well_known(sym::BOOL_METHOD),
            Sym::well_known(sym::HASH_METHOD),
            Sym::well_known(sym::EQ_METHOD),
            Sym::well_known(sym::LT_METHOD),
            Sym::well_known(sym::NEG_METHOD),
            Sym::well_known(sym::BNOT_METHOD),
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
        ];
        members.extend(
            this.inst_vtbl()
                .entries
                .iter()
                .filter_map(|(s, e)| match e {
                    Entry::Method(_)
                    | Entry::Getter(_)
                    | Entry::Property(_, _)
                    | Entry::Delegate(_) => Some(*s),
                    Entry::Setter(_) => None,
                }),
        );
        Some(Inspect {
            is_abstract: false,
            members,
        })
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ty = this.ty();
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
                            .call(this.as_header(), None, strand, args, out)
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
                    let ty = this.ty();
                    T::new(ty, strand, trailing, Slot::reborrow(&mut out)).await?;
                    let native = out.take();
                    let singleton = this.singleton(strand.vm());
                    inst.op_fill(strand, singleton, native)
                },
            )
            .await
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
            // Fall through to dispatch_native_method for protocol/special
            // methods (str, dbg, bool, hash, arithmetic, comparison, etc.).
            // It will error for unsupported operations.
            let singleton = this.singleton(strand.vm());
            dispatch_native_method(strand, singleton, method, args, out).await
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Check type-level entries first.
        if let Some(entry) = this.entry(field) {
            return match entry {
                Entry::Getter(handler) | Entry::Property(handler, _) => Strand::for_native_frame(
                    strand,
                    Cow::Borrowed(T::MODULE),
                    Cow::Borrowed(T::NAME),
                    Some(Cow::Borrowed("(get)")),
                    |strand| unsafe { handler.call(this.as_header(), strand, out) },
                ),
                Entry::Method(_) => {
                    BoundMethod::create(strand, &this, field, out);
                    Ok(())
                }
                Entry::Setter(_) | Entry::Delegate(_) => Err(Error::field(strand, field)),
            };
        }
        // Check instance-level entries (unbound methods/getters).
        if let Some(entry) = this.inst_entry(field) {
            return match entry {
                Entry::Method(_)
                | Entry::Getter(_)
                | Entry::Property(_, _)
                | Entry::Delegate(_) => {
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
            Err(Error::field(strand, field))
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
                    |strand| unsafe { handler.call(this.as_header(), strand, value) },
                ),
                _ => Err(Error::field(strand, field)),
            }
        } else {
            Err(Error::field(strand, field))
        }
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
        let ty = this.ty();
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
    let ty = unsafe { Type::<T>::from_type_header(header.cast()) };
    strand.pin_future_call(async move |strand| f(ty, strand, args, out).await)
}

/// Field glue for type-level closures: reconstructs `Type<T>` from the raw header.
unsafe fn type_field_glue<'v, 'a, 's, T: Object<'v>, F>(
    closure: NonNull<()>,
    header: NonNull<protocol::Header>,
    strand: &'a mut Strand<'v, 's>,
    slot: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    F: for<'ss> Fn(Type<'v, T>, &mut Strand<'v, 'ss>, Slot<'v, '_>) -> Result<'v, 'ss, ()> + 'v,
{
    let f = unsafe { &*closure.cast::<F>().as_ptr() };
    // Reconstruct Type<'v, T> from the type object header.
    let ty = unsafe { Type::<T>::from_type_header(header.cast()) };
    f(ty, strand, slot)
}
