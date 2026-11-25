//! VFS extension mechanism.
//!
//! An extension adds a new VFS operation from outside `dolang-vfs`,
//! dispatched identically whether the call is served in-process ("direct",
//! e.g. inside `dolang-shell`) or over a real RPC session ("remote", served
//! by `dolang-vfs`). Extensions do not get their own `dolang_rpc::Protocol`;
//! they ride as a single request/response variant in the crate-private VFS
//! protocol,
//! routed to the right handler by `(name, version)`.
//!
//! Extension authors implement [`VfsExtension`] and register it with
//! `vfs_extension!`. The macro links a `&'static dyn ErasedVfsExtension`
//! into a `linkme` distributed slice, so registration only requires linking
//! the extension crate into the binary — no explicit call site is needed,
//! and the same registration is picked up whether the binary serves direct
//! or remote requests (or both).
//!
//! This module is deliberately self-contained: nothing in the public API
//! (`ExtGift`, `ExtCite`, `ExtGuard`, `ExtResource`, `InvalidHandle`, `ExtOsHandle`,
//! `ExtContext`) names a `dolang_rpc` type. Extension crates should never
//! need to depend on `dolang-rpc` directly.

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    future::Future,
    pin::Pin,
    result,
    sync::{Arc, OnceLock},
};

use dolang_rpc::{
    handle::DefaultHandle,
    server::CallContext,
    session::{Cite, Gift, InvalidOpaque, OpaqueGuard, OpaqueResource},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    error::{Error, ErrorKind, Result},
    protocol::VfsProtocol,
};

/// VFS extension protocol versions supported by a backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionSet {
    versions: HashMap<String, Vec<u16>>,
}

impl ExtensionSet {
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (String, u16)>) -> Result<Self> {
        let mut versions: HashMap<String, Vec<u16>> = HashMap::new();
        for (name, version) in pairs {
            versions.entry(name).or_default().push(version);
        }
        for (name, versions) in &mut versions {
            versions.sort_unstable();
            if let Some(version) = versions
                .windows(2)
                .find_map(|pair| (pair[0] == pair[1]).then_some(pair[0]))
            {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    format!("duplicate VFS extension registration: {name} version {version}"),
                ));
            }
        }
        Ok(Self { versions })
    }

    /// Returns all supported versions for `name`, in ascending order.
    pub fn versions(&self, name: &str) -> Option<&[u16]> {
        self.versions.get(name).map(Vec::as_slice)
    }

    /// Returns whether the exact extension version is supported.
    pub fn supports(&self, name: &str, version: u16) -> bool {
        self.versions(name)
            .is_some_and(|versions| versions.binary_search(&version).is_ok())
    }

    /// Returns the highest version supported by both the backend and caller.
    pub fn maximum_common_version(&self, name: &str, supported: &[u16]) -> Option<u16> {
        let versions = self.versions(name)?;
        supported
            .iter()
            .copied()
            .filter(|version| versions.binary_search(version).is_ok())
            .max()
    }
}

#[doc(hidden)]
pub mod __private {
    #[allow(unused_imports)]
    pub use linkme;
}

/// A named, versioned VFS extension and its request handler.
///
/// Implement this trait for a zero-sized extension descriptor, then register
/// it with the `vfs_extension!` macro. The extension
/// must be linked into both the caller and the server for remote dispatch.
pub trait VfsExtension: Send + Sync + 'static {
    /// Extension request payload.
    type Request: Serialize + for<'de> Deserialize<'de> + Send + 'static;
    /// Extension response payload.
    type Response: Serialize + for<'de> Deserialize<'de> + Send + 'static;

    /// Extension name, used together with [`VERSION`](Self::VERSION) to route requests.
    const NAME: &'static str;
    /// Extension version, used together with [`NAME`](Self::NAME) to route requests.
    const VERSION: u16;
    /// Whether this process has a backend implementation for the extension.
    const AVAILABLE: bool = true;

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
    fn available(&self) -> bool;

    fn deserialize_request<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send>>;

    fn deserialize_response<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send>>;

    fn erase_request<'a>(&self, request: &'a (dyn Any + Send)) -> &'a dyn erased_serde::Serialize;

    fn erase_response<'a>(&self, response: &'a (dyn Any + Send))
    -> &'a dyn erased_serde::Serialize;

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExtContext<'_>,
        request: Box<dyn Any + Send>,
    ) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send + 'a>>;
}

impl<T: VfsExtension> ErasedVfsExtension for T {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn version(&self) -> u16 {
        T::VERSION
    }

    fn available(&self) -> bool {
        T::AVAILABLE
    }

    fn deserialize_request<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send>> {
        Ok(Box::new(erased_serde::deserialize::<T::Request>(de)?))
    }

    fn deserialize_response<'de>(
        &self,
        de: &mut dyn erased_serde::Deserializer<'de>,
    ) -> erased_serde::Result<Box<dyn Any + Send>> {
        Ok(Box::new(erased_serde::deserialize::<T::Response>(de)?))
    }

    fn erase_request<'a>(&self, request: &'a (dyn Any + Send)) -> &'a dyn erased_serde::Serialize {
        request
            .downcast_ref::<T::Request>()
            .expect("request type matches the routed extension")
    }

    fn erase_response<'a>(
        &self,
        response: &'a (dyn Any + Send),
    ) -> &'a dyn erased_serde::Serialize {
        response
            .downcast_ref::<T::Response>()
            .expect("response type matches the routed extension")
    }

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExtContext<'_>,
        request: Box<dyn Any + Send>,
    ) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send + 'a>> {
        let request = *request
            .downcast::<T::Request>()
            .expect("request type matches the routed extension");
        Box::pin(async move {
            let response = self.handle(ctx, request).await;
            Box::new(response) as Box<dyn Any + Send>
        })
    }
}

/// Registry of linked VFS extensions.
#[doc(hidden)]
#[linkme::distributed_slice]
pub static VFS_EXTENSIONS: [&'static dyn ErasedVfsExtension];

struct Registry {
    capabilities: ExtensionSet,
    handlers: HashMap<(&'static str, u16), &'static dyn ErasedVfsExtension>,
}

static REGISTERED: OnceLock<Result<Registry>> = OnceLock::new();

fn registry() -> Result<&'static Registry> {
    REGISTERED
        .get_or_init(|| {
            let mut handlers = HashMap::new();
            let extensions = VFS_EXTENSIONS.iter().copied();
            for extension in extensions.clone() {
                if handlers
                    .insert((extension.name(), extension.version()), extension)
                    .is_some()
                {
                    return Err(Error::new(
                        ErrorKind::AlreadyExists,
                        format!(
                            "duplicate VFS extension registration: {} version {}",
                            extension.name(),
                            extension.version()
                        ),
                    ));
                }
            }
            let capabilities = ExtensionSet::from_pairs(
                extensions
                    .filter(|extension| extension.available())
                    .map(|extension| (extension.name().to_owned(), extension.version())),
            )?;
            Ok(Registry {
                capabilities,
                handlers,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub(crate) fn registered() -> Result<&'static ExtensionSet> {
    Ok(&registry()?.capabilities)
}

/// Links a [`VfsExtension`]'s wire codec and, when
/// [`AVAILABLE`](VfsExtension::AVAILABLE), its backend handler.
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

pub use crate::vfs_extension;

/// Looks up a registered extension by name and version.
pub(crate) fn lookup(name: &str, version: u16) -> Option<&'static dyn ErasedVfsExtension> {
    registry().ok()?.handlers.get(&(name, version)).copied()
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
/// pattern used elsewhere in this crate (e.g. `AnyVfs`, `AnyFile`).
///
/// The direct/remote backing types are intentionally private, so extension
/// code can use this one context without depending on the crate's wire
/// protocol.
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
    ) -> result::Result<T, dolang_rpc::server::RequestCancelled>
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
    ///
    /// The result is an [`ExtGift`], which belongs in a wire position that
    /// hands the peer a reference. The peer names it back with
    /// [`ExtGift::cite`], and only the resulting [`ExtCite`] can be acquired.
    ///
    /// # Panics
    ///
    /// In remote mode, if a different concrete type has already been
    /// registered under `T::Marker` on this session: a marker must name
    /// exactly one resource type, since it is the only type information that
    /// crosses the wire.
    pub fn register<T: ExtResource>(&self, value: T) -> ExtGift<T::Marker> {
        match &self.inner {
            Inner::Direct(_) => ExtGift(GiftRepr::Direct(Arc::new(value))),
            Inner::Remote { context, .. } => {
                ExtGift(GiftRepr::Remote(context.register(Wrap(value))))
            }
        }
    }

    /// Resolves a citation of a handle previously returned by
    /// [`register`](Self::register).
    pub fn acquire<T: ExtResource>(
        &self,
        handle: ExtCite<T::Marker>,
    ) -> result::Result<ExtGuard<T>, InvalidHandle> {
        match (&self.inner, handle.0) {
            (Inner::Direct(_), CiteRepr::Direct(value)) => {
                if (*value).type_id() != TypeId::of::<T>() {
                    return Err(InvalidHandle);
                }
                Ok(ExtGuard(GuardRepr::Direct(
                    value.downcast::<T>().map_err(|_| InvalidHandle)?,
                )))
            }
            (Inner::Remote { context, .. }, CiteRepr::Remote(cite)) => Ok(ExtGuard(
                GuardRepr::Remote(context.acquire::<Wrap<T>>(cite)?),
            )),
            _ => Err(InvalidHandle),
        }
    }

    /// Removes a handle previously returned by [`register`](Self::register),
    /// returning the stored value if this was the last reference to it.
    pub fn unregister<T: ExtResource>(
        &self,
        handle: ExtCite<T::Marker>,
    ) -> result::Result<Option<T>, InvalidHandle> {
        match (&self.inner, handle.0) {
            (Inner::Direct(_), CiteRepr::Direct(value)) => {
                if (*value).type_id() != TypeId::of::<T>() {
                    return Err(InvalidHandle);
                }
                let value = value.downcast::<T>().map_err(|_| InvalidHandle)?;
                Ok(Arc::try_unwrap(value).ok())
            }
            (Inner::Remote { context, .. }, CiteRepr::Remote(cite)) => {
                Ok(context.unregister::<Wrap<T>>(cite)?.map(|w| w.0))
            }
            _ => Err(InvalidHandle),
        }
    }
}

/// A value that can be registered in an extension's opaque-object table via
/// [`ExtContext::register`].
///
/// This mirrors `dolang_rpc::session::OpaqueResource`, which extension authors do not
/// implement directly — that would require depending on `dolang-rpc` and
/// would leak its `Marker`-keyed object-table design into every extension
/// crate's own trait-impl list.
pub trait ExtResource: Send + Sync + 'static {
    /// A trivial type naming this resource on the wire. It must name only
    /// this one — see [`ExtContext::register`].
    type Marker: 'static;
}

/// Private adapter bridging [`ExtResource`] to `dolang_rpc::session::OpaqueResource`
/// so [`ExtContext`] can delegate to `CallContext`'s real object table.
struct Wrap<T>(T);

impl<T: ExtResource> OpaqueResource for Wrap<T> {
    type Marker = T::Marker;
}

/// A handle to a value registered via [`ExtContext::register`], in a wire
/// position that grants the peer a reference to it.
///
/// Uses a distinct `Marker` type parameter rather than the concrete stored
/// type so the handle a caller holds does not need to name (or even know)
/// the private type actually retained behind it — the same design
/// `dolang_rpc::session::Gift` uses for its own object table.
///
/// Opaque by design: the direct/remote split is an implementation detail,
/// not something extension authors match on.
pub struct ExtGift<M: 'static>(GiftRepr<M>);

/// The same handle in a wire position that names a reference the receiver has
/// already granted, produced by [`ExtGift::cite`].
///
/// Which of the two a protocol field holds is fixed by the protocol, so it is
/// spelled in the field's type and checked when the field is decoded. See
/// `dolang_rpc::session` for what the distinction buys.
pub struct ExtCite<M: 'static>(CiteRepr<M>);

enum GiftRepr<M: 'static> {
    Direct(Arc<dyn Any + Send + Sync>),
    Remote(Gift<M>),
}

enum CiteRepr<M: 'static> {
    Direct(Arc<dyn Any + Send + Sync>),
    Remote(Cite<M>),
}

impl<M> ExtGift<M> {
    /// Names this resource back to the endpoint that owns it, for a wire
    /// position that must not transfer a reference.
    ///
    /// # Panics
    ///
    /// In remote mode, if this endpoint is itself the owner — see
    /// `dolang_rpc::session::Gift::cite`. Direct mode has no wire and no
    /// owner, so it cannot fail this way; extensions should still route
    /// through here so that the two modes agree.
    pub fn cite(&self) -> ExtCite<M> {
        match &self.0 {
            GiftRepr::Direct(value) => ExtCite(CiteRepr::Direct(value.clone())),
            GiftRepr::Remote(gift) => ExtCite(CiteRepr::Remote(gift.cite())),
        }
    }
}

/// Both handles are the same value in different wire positions, so everything
/// that does not touch the wire is identical between them.
macro_rules! ext_handle {
    ($name:ident, $repr:ident) => {
        impl<M> Clone for $name<M> {
            fn clone(&self) -> Self {
                match &self.0 {
                    $repr::Direct(value) => Self($repr::Direct(value.clone())),
                    $repr::Remote(handle) => Self($repr::Remote(handle.clone())),
                }
            }
        }

        impl<M: 'static> Serialize for $name<M> {
            fn serialize<S: Serializer>(&self, serializer: S) -> result::Result<S::Ok, S::Error> {
                match &self.0 {
                    $repr::Remote(handle) => handle.serialize(serializer),
                    $repr::Direct(_) => Err(serde::ser::Error::custom(
                        "cannot serialize a direct-mode extension handle",
                    )),
                }
            }
        }
    };
}

ext_handle!(ExtGift, GiftRepr);
ext_handle!(ExtCite, CiteRepr);

impl<'de, M: 'static> Deserialize<'de> for ExtGift<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> result::Result<Self, D::Error> {
        Gift::<M>::deserialize(deserializer).map(|gift| Self(GiftRepr::Remote(gift)))
    }
}

impl<'de, M: 'static> Deserialize<'de> for ExtCite<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> result::Result<Self, D::Error> {
        Cite::<M>::deserialize(deserializer).map(|cite| Self(CiteRepr::Remote(cite)))
    }
}

/// A retained, typed handle acquired via [`ExtContext::acquire`].
///
/// Opaque by design, for the same reason as [`ExtGift`].
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

/// Error returned when an [`ExtGift`]/[`ExtCite`] does not refer to a live,
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
/// Self-contained wrapper around `dolang_rpc::handle::OsHandle`: constructing or
/// consuming one never requires an [`ExtContext`] — by the time a value is
/// deserialized (a client reading a response, or a handler reading a
/// request field), any attachment has already been resolved into a concrete
/// local handle. Only *encoding a response* that carries one should be
/// gated by [`ExtContext::native_capable`] first, since the underlying
/// transport panics on attachment attempts if it does not support them.
pub struct ExtOsHandle(dolang_rpc::handle::OsHandle);

impl ExtOsHandle {
    /// Wraps a native handle for an extension response or request.
    pub fn new(handle: DefaultHandle) -> Self {
        Self(dolang_rpc::handle::OsHandle::new(handle))
    }

    /// Returns the wrapped native handle.
    pub fn into_inner(self) -> DefaultHandle {
        self.0.into_inner()
    }
}

impl Serialize for ExtOsHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtOsHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> result::Result<Self, D::Error> {
        dolang_rpc::handle::OsHandle::deserialize(deserializer).map(Self)
    }
}
