//! Querying values for Rust interfaces by tag.
//!
//! A consumer asks a [`Value`] for an interface with [`Value::query`], naming it by an
//! [`InterfaceTag`]. The query visits the value's providers ([`Object::provide`] on a native
//! object, or each native super of a Do class instance) with a [`Request`]. A provider that
//! recognizes the tag lends the interface to the consumer's closure for the duration of the
//! call, typically from inside a borrow of its state.
//!
//! [`Object::provide`]: super::native::Object::provide

use std::{
    any::TypeId,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    error::{Error, Result},
    object::protocol::Dispatch,
    strand::Strand,
    value::{Case, Value},
};

/// Names the interface type provided under a unique tag.
///
/// The tag is a key, usually a unit struct; its [`TypeId`] identifies the request. The
/// interface is usually a trait object, such as `dyn Trait<'v> + 'a`, where `'a` is the
/// lend: a provider can lend a value that borrows from its receiver or its stack.
pub trait InterfaceTag: 'static {
    type Interface<'v, 'a>: ?Sized;
}

/// An outstanding interface request, passed to providers.
///
/// Dereferences to the requesting [`Strand`], so a provider can borrow its state with it.
pub struct Request<'v, 's, 'r> {
    strand: &'r mut Strand<'v, 's>,
    answer: &'r mut dyn Answer<'v, 's>,
}

impl<'v, 's, 'r> Request<'v, 's, 'r> {
    pub(crate) fn new(strand: &'r mut Strand<'v, 's>, answer: &'r mut dyn Answer<'v, 's>) -> Self {
        Self { strand, answer }
    }

    /// Splits the request so it can be rebuilt around a derived strand.
    pub(crate) fn parts(&mut self) -> (&mut Strand<'v, 's>, &mut dyn Answer<'v, 's>) {
        (&mut *self.strand, &mut *self.answer)
    }

    /// Whether the request is for `K` and has not been satisfied.
    pub fn wants<K: InterfaceTag>(&self) -> bool {
        !self.answer.is_done() && self.answer.tag() == TypeId::of::<K>()
    }

    /// Lends `iface` to the consumer if the request [wants](Request::wants) `K`, and does
    /// nothing otherwise.
    ///
    /// Returns the consumer's error, which the provider should propagate.
    pub fn provide<K: InterfaceTag>(
        &mut self,
        iface: &mut K::Interface<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if !self.wants::<K>() {
            return Ok(());
        }
        let mut iface = iface;
        // SAFETY: `wants` checked the tag, so the answer expects `&mut K::Interface<'v, '_>`.
        unsafe {
            self.answer
                .answer(&mut *self.strand, NonNull::from(&mut iface).cast())
        }
    }
}

impl<'v, 's> Deref for Request<'v, 's, '_> {
    type Target = Strand<'v, 's>;

    fn deref(&self) -> &Strand<'v, 's> {
        &*self.strand
    }
}

impl<'v, 's> DerefMut for Request<'v, 's, '_> {
    fn deref_mut(&mut self) -> &mut Strand<'v, 's> {
        &mut *self.strand
    }
}

impl<'v, 's> AsRef<Strand<'v, 's>> for Request<'v, 's, '_> {
    fn as_ref(&self) -> &Strand<'v, 's> {
        &*self.strand
    }
}

impl<'v, 's> AsMut<Strand<'v, 's>> for Request<'v, 's, '_> {
    fn as_mut(&mut self) -> &mut Strand<'v, 's> {
        &mut *self.strand
    }
}

/// The consumer side of a request, erased over the tag and closure.
pub(crate) trait Answer<'v, 's> {
    fn tag(&self) -> TypeId;

    fn is_done(&self) -> bool;

    /// Calls the consumer with the interface.
    ///
    /// # Safety
    ///
    /// `iface` must point to a `&mut K::Interface<'v, '_>`, where `K` has the [`TypeId`]
    /// returned by [`Answer::tag`].
    unsafe fn answer(
        &mut self,
        strand: &mut Strand<'v, 's>,
        iface: NonNull<()>,
    ) -> Result<'v, 's, ()>;
}

struct Query<K, F, R> {
    consumer: Option<F>,
    result: Option<R>,
    tag: PhantomData<K>,
}

impl<'v, 's, K, F, R> Answer<'v, 's> for Query<K, F, R>
where
    K: InterfaceTag,
    F: for<'a> FnOnce(&mut Strand<'v, 's>, &mut K::Interface<'v, 'a>) -> Result<'v, 's, R>,
{
    fn tag(&self) -> TypeId {
        TypeId::of::<K>()
    }

    fn is_done(&self) -> bool {
        self.consumer.is_none()
    }

    unsafe fn answer(
        &mut self,
        strand: &mut Strand<'v, 's>,
        iface: NonNull<()>,
    ) -> Result<'v, 's, ()> {
        // SAFETY: guaranteed by the caller.
        let iface = unsafe { lent::<K>(iface) };
        let consumer = self.consumer.take().expect("request already answered");
        self.result = Some(consumer(strand, iface)?);
        Ok(())
    }
}

/// Reads the interface `iface` points to, for a lend no longer than the call.
///
/// # Safety
///
/// `iface` must point to a live `&mut K::Interface<'v, 'b>` for some `'b` outliving `'a`.
unsafe fn lent<'a, 'v, K: InterfaceTag>(iface: NonNull<()>) -> &'a mut K::Interface<'v, 'a> {
    // SAFETY: guaranteed by the caller. Shortening the lend to `'a` only narrows what the
    // consumer may assume.
    unsafe { &mut **iface.cast::<&mut K::Interface<'v, 'a>>().as_ptr() }
}

pub(crate) fn query<'v, 's, K: InterfaceTag, R>(
    value: &Value<'v>,
    strand: &mut Strand<'v, 's>,
    consumer: impl for<'a> FnOnce(&mut Strand<'v, 's>, &mut K::Interface<'v, 'a>) -> Result<'v, 's, R>,
) -> Result<'v, 's, R> {
    let mut query = Query::<K, _, _> {
        consumer: Some(consumer),
        result: None,
        tag: PhantomData,
    };
    if let Case::Object(o) = value.case() {
        o.op_provide(&mut Request::new(strand, &mut query))?;
    }
    query.result.ok_or_else(|| Error::not_supported(strand))
}
