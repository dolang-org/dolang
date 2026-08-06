//! VFS extension mechanism.
//!
//! An extension adds a new VFS operation from outside `dolang-vfs`,
//! dispatched identically whether the call is served in-process ("direct",
//! e.g. inside `dolang-shell`) or over a real RPC session ("remote", served
//! by `dolang-vfs`). Extensions do not get their own `dolang_rpc::Protocol`;
//! they ride as a single request/response variant carried by [`VfsProtocol`],
//! routed to the right handler by `(name, version)`.
//!
//! Extension authors implement [`VfsExtension`] and register it with
//! [`vfs_extension!`]. The macro links a `&'static dyn ErasedVfsExtension`
//! into a `linkme` distributed slice, so registration only requires linking
//! the extension crate into the binary — no explicit call site is needed,
//! and the same registration is picked up whether the binary serves direct
//! or remote requests (or both).
//!
//! This module is deliberately self-contained: nothing in the public API
//! (`ExtOpaque`, `ExtGuard`, `ExtResource`, `InvalidHandle`, `ExtOsHandle`,
//! `ExtContext`) names a `dolang_rpc` type. Extension crates should never
//! need to depend on `dolang-rpc` directly.

use std::{
    any::{Any, TypeId},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use dolang_rpc::{CallContext, DefaultHandle, InvalidOpaque, Opaque, OpaqueGuard, OpaqueResource};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::protocol::VfsProtocol;

#[doc(hidden)]
pub mod __private {
    #[allow(unused_imports)]
    pub use linkme;
}

/// A single VFS extension: a named, versioned request/response pair plus a handler.
pub trait VfsExtension: Send + Sync + 'static {
    /// Extension request payload.
    type Request: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static;
    /// Extension response payload.
    type Response: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static;

    /// Extension name, used together with [`VERSION`](Self::VERSION) to route requests.
    const NAME: &'static str;
    /// Extension version, used together with [`NAME`](Self::NAME) to route requests.
    const VERSION: u16;

    /// Handles a single request.
    fn handle(
        &self,
        ctx: &mut ExtContext<'_>,
        request: Self::Request,
    ) -> impl Future<Output = Self::Response> + Send;
}

/// Object-safe, type-erased view of a [`VfsExtension`].
///
/// Generated automatically for every `T: VfsExtension` by a blanket impl;
/// extension authors never implement this directly.
#[doc(hidden)]
pub trait ErasedVfsExtension: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn version(&self) -> u16;

    fn deserialize_request<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send + Sync>>;

    fn deserialize_response<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send + Sync>>;

    fn erase_request<'a>(
        &self,
        request: &'a (dyn Any + Send + Sync),
    ) -> &'a dyn erased_serde::Serialize;

    fn erase_response<'a>(
        &self,
        response: &'a (dyn Any + Send + Sync),
    ) -> &'a dyn erased_serde::Serialize;

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExtContext<'_>,
        request: Box<dyn Any + Send + Sync>,
    ) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send + Sync>> + Send + 'a>>;
}

impl<T: VfsExtension> ErasedVfsExtension for T {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn version(&self) -> u16 {
        T::VERSION
    }

    fn deserialize_request<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send + Sync>> {
        Ok(Box::new(erased_serde::deserialize::<T::Request>(de)?))
    }

    fn deserialize_response<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send + Sync>> {
        Ok(Box::new(erased_serde::deserialize::<T::Response>(de)?))
    }

    fn erase_request<'a>(
        &self,
        request: &'a (dyn Any + Send + Sync),
    ) -> &'a dyn erased_serde::Serialize {
        request
            .downcast_ref::<T::Request>()
            .expect("request type matches the routed extension")
    }

    fn erase_response<'a>(
        &self,
        response: &'a (dyn Any + Send + Sync),
    ) -> &'a dyn erased_serde::Serialize {
        response
            .downcast_ref::<T::Response>()
            .expect("response type matches the routed extension")
    }

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExtContext<'_>,
        request: Box<dyn Any + Send + Sync>,
    ) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send + Sync>> + Send + 'a>> {
        let request = *request
            .downcast::<T::Request>()
            .expect("request type matches the routed extension");
        Box::pin(async move {
            let response = self.handle(ctx, request).await;
            Box::new(response) as Box<dyn Any + Send + Sync>
        })
    }
}

/// Registry of linked VFS extensions.
#[doc(hidden)]
#[linkme::distributed_slice]
pub static VFS_EXTENSIONS: [&'static dyn ErasedVfsExtension];

/// Registers a [`VfsExtension`], making it available for both direct and
/// remote dispatch in any binary that links the crate calling this macro.
#[macro_export]
macro_rules! vfs_extension {
    ($expr:expr) => {
        #[$crate::extension::__private::linkme::distributed_slice(
            $crate::extension::VFS_EXTENSIONS
        )]
        #[linkme(crate = $crate::extension::__private::linkme)]
        static _VFS_EXTENSION: &'static dyn $crate::extension::ErasedVfsExtension = &$expr;
    };
}

/// Looks up a registered extension by name and version.
pub(crate) fn lookup(name: &str, version: u16) -> Option<&'static dyn ErasedVfsExtension> {
    VFS_EXTENSIONS
        .iter()
        .copied()
        .find(|ext| ext.name() == name && ext.version() == version)
}

/// State backing direct (in-process) extension dispatch.
///
/// Direct dispatch has no session or wire boundary, so unlike the remote
/// path it carries no cancellation-signal machinery: a caller cancels a
/// direct extension call the ordinary Rust way, by dropping the awaited
/// future, and that drop already propagates through any `.await` inside the
/// handler. [`ExtContext::cancel_guard`] on the direct path is
/// therefore just a passthrough, kept only so extension authors can write
/// one `cancel_guard` call that works, unmodified, under both dispatch modes.
#[derive(Default)]
pub struct DirectContext {
    _private: (),
}

/// Backend-agnostic context passed to [`VfsExtension::handle`].
///
/// Presents the same register/acquire/unregister/cancel_guard surface
/// regardless of whether the call arrived directly (in-process) or over a
/// real RPC session, mirroring the existing direct/remote enum-dispatch
/// pattern used elsewhere in this crate (e.g. `AnyVfs`, `ClientFileInner`).
///
/// The direct/remote split is deliberately not exposed as a public enum:
/// `CallContext<VfsProtocol>` (the remote backing type) can only be named
/// from outside this crate if `VfsProtocol`'s associated `Request`/
/// `Response` types are also public, which would leak this crate's private
/// wire protocol. Wrapping the split in a private `Inner` keeps `VfsProtocol`
/// itself `pub(crate)`.
pub struct ExtContext<'a> {
    inner: Inner<'a>,
}

enum Inner<'a> {
    Direct(&'a mut DirectContext),
    Remote {
        context: &'a mut CallContext<VfsProtocol>,
        native_capable: bool,
    },
}

impl<'a> ExtContext<'a> {
    pub(crate) fn direct(state: &'a mut DirectContext) -> Self {
        Self {
            inner: Inner::Direct(state),
        }
    }

    pub(crate) fn remote(context: &'a mut CallContext<VfsProtocol>, native_capable: bool) -> Self {
        Self {
            inner: Inner::Remote {
                context,
                native_capable,
            },
        }
    }

    /// Whether the peer's transport can carry native OS handles as
    /// out-of-band attachments (see [`ExtOsHandle`]).
    ///
    /// Always `false` for direct (in-process) dispatch — there is no wire
    /// boundary to cross, so [`register`](Self::register) already produces a
    /// zero-cost handle and there is nothing to gain from a native handle.
    pub fn native_capable(&self) -> bool {
        match &self.inner {
            Inner::Direct(_) => false,
            Inner::Remote { native_capable, .. } => *native_capable,
        }
    }

    /// Runs an operation which can observe request cancellation without
    /// dropping the handler.
    ///
    /// On the remote path this delegates to
    /// [`CallContext::cancel_guard`], which cooperatively signals
    /// cancellation requested by the peer. On the direct path this is a
    /// passthrough (see [`DirectContext`]).
    pub async fn cancel_guard<T, F>(
        &mut self,
        operation: F,
    ) -> Result<T, dolang_rpc::RequestCancelled>
    where
        F: for<'b> AsyncFnOnce(&'b mut ExtContext<'b>) -> T,
    {
        match &mut self.inner {
            Inner::Direct(state) => {
                let mut ctx = ExtContext::direct(state);
                Ok(operation(&mut ctx).await)
            }
            Inner::Remote {
                context,
                native_capable,
            } => {
                let native_capable = *native_capable;
                context
                    .cancel_guard(async move |context| {
                        let mut ctx = ExtContext::remote(context, native_capable);
                        operation(&mut ctx).await
                    })
                    .await
            }
        }
    }

    /// Registers a value in the session's opaque-object table, returning a
    /// handle that can cross the wire (when remote) and be redeemed with
    /// [`acquire`](Self::acquire)/[`unregister`](Self::unregister).
    pub fn register<T: ExtResource>(&self, value: T) -> ExtOpaque<T::Marker> {
        match &self.inner {
            Inner::Direct(_) => ExtOpaque(OpaqueRepr::Direct(Arc::new(value))),
            Inner::Remote { context, .. } => {
                ExtOpaque(OpaqueRepr::Remote(context.register(Wrap(value))))
            }
        }
    }

    /// Resolves a handle previously returned by [`register`](Self::register).
    pub fn acquire<T: ExtResource>(
        &self,
        handle: ExtOpaque<T::Marker>,
    ) -> Result<ExtGuard<T>, InvalidHandle> {
        match (&self.inner, handle.0) {
            (Inner::Direct(_), OpaqueRepr::Direct(value)) => {
                if (*value).type_id() != TypeId::of::<T>() {
                    return Err(InvalidHandle);
                }
                Ok(ExtGuard(GuardRepr::Direct(
                    value.downcast::<T>().map_err(|_| InvalidHandle)?,
                )))
            }
            (Inner::Remote { context, .. }, OpaqueRepr::Remote(opaque)) => Ok(ExtGuard(
                GuardRepr::Remote(context.acquire::<Wrap<T>>(opaque)?),
            )),
            _ => Err(InvalidHandle),
        }
    }

    /// Removes a handle previously returned by [`register`](Self::register),
    /// returning the stored value if this was the last reference to it.
    pub fn unregister<T: ExtResource>(
        &self,
        handle: ExtOpaque<T::Marker>,
    ) -> Result<Option<T>, InvalidHandle> {
        match (&self.inner, handle.0) {
            (Inner::Direct(_), OpaqueRepr::Direct(value)) => {
                if (*value).type_id() != TypeId::of::<T>() {
                    return Err(InvalidHandle);
                }
                let value = value.downcast::<T>().map_err(|_| InvalidHandle)?;
                Ok(Arc::try_unwrap(value).ok())
            }
            (Inner::Remote { context, .. }, OpaqueRepr::Remote(opaque)) => {
                Ok(context.unregister::<Wrap<T>>(opaque)?.map(|w| w.0))
            }
            _ => Err(InvalidHandle),
        }
    }
}

/// A value that can be registered in an extension's opaque-object table via
/// [`ExtContext::register`].
///
/// This mirrors `dolang_rpc::OpaqueResource`, which extension authors do not
/// implement directly — that would require depending on `dolang-rpc` and
/// would leak its `Marker`-keyed object-table design into every extension
/// crate's own trait-impl list.
pub trait ExtResource: Send + Sync + 'static {
    type Marker: ?Sized + 'static;
}

/// Private adapter bridging [`ExtResource`] to `dolang_rpc::OpaqueResource`
/// so [`ExtContext`] can delegate to `CallContext`'s real object table.
struct Wrap<T>(T);

impl<T: ExtResource> OpaqueResource for Wrap<T> {
    type Marker = T::Marker;
}

/// A handle to a value registered via [`ExtContext::register`].
///
/// Uses a distinct `Marker` type parameter rather than the concrete stored
/// type so the handle a caller holds does not need to name (or even know)
/// the private type actually retained behind it — the same design
/// `dolang_rpc::Opaque` uses for its own object table.
///
/// Opaque by design: the direct/remote split is an implementation detail,
/// not something extension authors match on.
pub struct ExtOpaque<M: ?Sized + 'static>(OpaqueRepr<M>);

enum OpaqueRepr<M: ?Sized + 'static> {
    Direct(Arc<dyn Any + Send + Sync>),
    Remote(Opaque<M>),
}

impl<M: ?Sized> Clone for ExtOpaque<M> {
    fn clone(&self) -> Self {
        match &self.0 {
            OpaqueRepr::Direct(value) => Self(OpaqueRepr::Direct(value.clone())),
            OpaqueRepr::Remote(opaque) => Self(OpaqueRepr::Remote(opaque.clone())),
        }
    }
}

impl<M: ?Sized + 'static> Serialize for ExtOpaque<M> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            OpaqueRepr::Remote(opaque) => opaque.serialize(serializer),
            OpaqueRepr::Direct(_) => Err(serde::ser::Error::custom(
                "cannot serialize a direct-mode extension handle",
            )),
        }
    }
}

impl<'de, M: ?Sized + 'static> Deserialize<'de> for ExtOpaque<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Opaque::<M>::deserialize(deserializer).map(|opaque| Self(OpaqueRepr::Remote(opaque)))
    }
}

/// A retained, typed handle acquired via [`ExtContext::acquire`].
///
/// Opaque by design, for the same reason as [`ExtOpaque`].
pub struct ExtGuard<T>(GuardRepr<T>);

enum GuardRepr<T> {
    Direct(Arc<T>),
    Remote(OpaqueGuard<Wrap<T>>),
}

impl<T> std::ops::Deref for ExtGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.0 {
            GuardRepr::Direct(value) => value,
            GuardRepr::Remote(guard) => &guard.deref().0,
        }
    }
}

/// Error returned when an [`ExtOpaque`]/handle does not refer to a live,
/// correctly-typed value.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid extension handle")]
pub struct InvalidHandle;

impl From<InvalidOpaque> for InvalidHandle {
    fn from(_: InvalidOpaque) -> Self {
        InvalidHandle
    }
}

/// A native OS handle carried as an out-of-band attachment on the wire.
///
/// Self-contained wrapper around `dolang_rpc::OsHandle`: constructing or
/// consuming one never requires an [`ExtContext`] — by the time a value is
/// deserialized (a client reading a response, or a handler reading a
/// request field), any attachment has already been resolved into a concrete
/// local handle. Only *encoding a response* that carries one should be
/// gated by [`ExtContext::native_capable`] first, since the underlying
/// transport panics on attachment attempts if it does not support them.
pub struct ExtOsHandle(dolang_rpc::OsHandle);

impl ExtOsHandle {
    pub fn new(handle: DefaultHandle) -> Self {
        Self(dolang_rpc::OsHandle::new(handle))
    }

    pub fn into_inner(self) -> DefaultHandle {
        self.0.into_inner()
    }
}

impl Serialize for ExtOsHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtOsHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        dolang_rpc::OsHandle::deserialize(deserializer).map(Self)
    }
}
