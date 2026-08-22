//! Session-scoped opaque handles.
//!
//! [`Gift`] is an opaque resource given to a peer by handle, and [`Cite`]
//! if a reference to such a resource.  This can be used to represent any
//! sort of resource that does not pass between client and server directly,
//! such as open files.  They are only valid for a particular RPC session.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(unix)]
use std::os::fd::OwnedFd;
use std::{
    any::{Any, TypeId},
    collections::{HashMap, hash_map::Entry},
    fmt, io,
    marker::PhantomData,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use crate::{handle::TakeHandle, transport::ReceivedHandles};
use crate::{
    handle::{ErasedHandle, PutHandle},
    transport::EncodeHandles,
};

/// The owner bit rides the low bit of the id.
pub(crate) fn pack_wire(owner: u8, id: u64) -> u64 {
    debug_assert!(id < (1 << 63), "opaque identifier is too large to pack");
    (id << 1) | u64::from(owner & 1)
}

pub(crate) fn unpack_wire(packed: u64) -> (u8, u64) {
    ((packed & 1) as u8, packed >> 1)
}

/// Wire discriminant: the resource belongs to the sender.
const WIRE_GIFT: u8 = 0;
/// Wire discriminant: the resource belongs to the receiver.
const WIRE_CITATION: u8 = 1;

/// Collapse a remote grant total before it can approach the integer ceiling.
/// One reference is retained for the live local handle; the rest are returned
/// to the owner in one counted release.
const GRANT_RELEASE_THRESHOLD: u32 = u32::MAX / 2;

/// A value that can be registered in a session's opaque-object table.
///
/// `Marker` is the public protocol-level type carried by [`Gift`] and [`Cite`];
/// it is nothing but a name, so that the concrete resource type may remain
/// private.
/// The mapping from marker to resource must be injective, and
/// `Session::register` panics if an application ever registers two concrete
/// types under one marker — otherwise a marker would not identify a type and
/// the wire could not be typechecked at all.
pub trait OpaqueResource: Send + Sync + 'static {
    type Marker: 'static;
}

/// Emits release frames for opaques whose last local handle has dropped.
///
/// Implemented on each endpoint's `WeakUnboundedSender` for its own outgoing
/// message type. Sending must not block or fail loudly: this is called from
/// `Drop`. Deliberately weak: a writer task treats "every sender dropped" as
/// its shutdown signal and transitively holds the `Session` that owns its
/// sink, so a strong sender here would be a cycle — the writer waiting on a
/// channel it is itself keeping open.
pub(crate) trait ReleaseSink: Send + Sync + 'static {
    fn release(&self, id: u64, count: u32);
}

/// A handle on a resource this endpoint owns.
///
/// The `Arc` around it is the local handle count. The resource itself lives in
/// the session table, never in here, so that [`Session::unregister`] can empty
/// the slot and have every outstanding handle observe the revocation. That
/// is the whole reason `Session::acquire` is fallible: resolving an opaque is
/// `open()` on a descriptor number, not a pointer dereference.
pub(crate) struct LocalRef {
    id: u64,
    /// Which session minted this handle. An id means nothing outside the
    /// session that issued it, so every redemption checks it.
    serial: u64,
    session: Weak<Session>,
}

/// A handle on a resource the peer owns.
///
/// The protocol count lives in the table entry, not here: it is the total the
/// peer has granted for the id, and whichever handle is alive owns that whole
/// total. See [`RemoteRef::drop`] for how a handle that loses a race forfeits
/// it rather than splitting it.
pub(crate) struct RemoteRef {
    id: u64,
    /// See [`LocalRef::serial`].
    serial: u64,
    session: Weak<Session>,
}

/// The handle behind a [`Gift`] or a [`Cite`], which differ only in the wire
/// position they are legal in — this carries everything either of them does.
pub(crate) enum Inner {
    Local(Arc<LocalRef>),
    Remote(Arc<RemoteRef>),
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        // Purely a local handle count bump; the protocol count is untouched.
        match self {
            Self::Local(local) => Self::Local(local.clone()),
            Self::Remote(remote) => Self::Remote(remote.clone()),
        }
    }
}

impl Inner {
    fn id(&self) -> u64 {
        match self {
            Self::Local(local) => local.id,
            Self::Remote(remote) => remote.id,
        }
    }

    fn owner(&self) -> u8 {
        match self {
            Self::Local(_) => WIRE_GIFT,
            Self::Remote(_) => WIRE_CITATION,
        }
    }
}

impl Drop for LocalRef {
    fn drop(&mut self) {
        // A dead `Weak<Session>` means the connection itself is tearing down,
        // which retires every table wholesale. Nothing to do.
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let mut tables = session.tables.lock().unwrap();
        let Some(entry) = tables.local.get(&self.id) else {
            return;
        };
        // Only act if the entry still points at *us*: a citation that arrived
        // while this handle was dying installed a fresh one (see `cite`), and
        // that one now owns the registration.
        if !entry.points_at(self) {
            return;
        }
        // The peer may still name this resource even though we no longer hold
        // a handle on it, in which case the entry (and the resource) has to
        // outlive us and is retired by the final release instead.
        if entry.granted == 0 {
            tables.local.remove(&self.id);
        }
    }
}

impl Drop for RemoteRef {
    fn drop(&mut self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let granted = {
            let mut tables = session.tables.lock().unwrap();
            // Only act if the slot still points at *us*. A gift that failed to
            // upgrade this handle mid-drop installed a fresh one and folded
            // our references into the entry's running total; that handle now
            // owns the whole total, so this one releases nothing.
            if !tables
                .remote
                .get(&self.id)
                .is_some_and(|entry| entry.points_at(self))
            {
                return;
            }
            tables
                .remote
                .remove(&self.id)
                .expect("just matched")
                .granted
        };
        if granted > 0 {
            session.sink.release(self.id, granted);
        }
    }
}

/// Panic message shared by the two places that catch the same mistake.
const CITE_OWNED: &str = "cannot cite a resource this endpoint owns; \
                          gift it again to name it to the peer";

/// An opaque handle that is granted to the client.  The server
/// owns the resource, and the client obtains a handle to it for
/// subsequent use with [`Gift::cite`].
pub struct Gift<M> {
    pub(crate) inner: Inner,
    marker: PhantomData<fn() -> M>,
}

/// An reference to a previously-granted opaque handle.
///
/// Produced by [`Gift::cite`].
pub struct Cite<M> {
    pub(crate) inner: Inner,
    marker: PhantomData<fn() -> M>,
}

impl<M> Cite<M> {
    pub(crate) fn new(inner: Inner) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }
}

impl<M> Gift<M> {
    pub(crate) fn new(inner: Inner) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }

    /// Creates a citation handle for sending back to the server.
    ///
    /// # Panics
    ///
    /// If used by the server on a handle it registered itself.
    pub fn cite(&self) -> Cite<M> {
        assert!(matches!(self.inner, Inner::Remote(_)), "{CITE_OWNED}");
        Cite {
            inner: self.inner.clone(),
            marker: PhantomData,
        }
    }
}

/// The two handle types differ only in the wire position they are legal in, so
/// everything that does not touch the wire is identical between them.
macro_rules! opaque_handle {
    ($name:ident) => {
        impl<M> Clone for $name<M> {
            fn clone(&self) -> Self {
                Self {
                    inner: self.inner.clone(),
                    marker: PhantomData,
                }
            }
        }

        impl<M> PartialEq for $name<M> {
            fn eq(&self, other: &Self) -> bool {
                self.inner.owner() == other.inner.owner() && self.inner.id() == other.inner.id()
            }
        }

        impl<M> Eq for $name<M> {}

        impl<M> fmt::Debug for $name<M> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("owner", &self.inner.owner())
                    .field("id", &self.inner.id())
                    .finish_non_exhaustive()
            }
        }
    };
}

opaque_handle!(Gift);
opaque_handle!(Cite);

impl<M> Serialize for Gift<M> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        assert!(
            matches!(self.inner, Inner::Local(_)),
            "cannot gift a resource this endpoint does not own; \
             use `Gift::cite` to name it back to its owner"
        );
        crate::serde::serialize_opaque(&self.inner, serializer)
    }
}

impl<'de, M> Deserialize<'de> for Gift<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // No marker check: the peer's table is the authority on the type of a
        // resource the peer owns, and this side has nothing to check it against.
        crate::serde::deserialize_gift(deserializer).map(Gift::new)
    }
}

impl<M> Serialize for Cite<M> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        assert!(matches!(self.inner, Inner::Remote(_)), "{CITE_OWNED}");
        crate::serde::serialize_opaque(&self.inner, serializer)
    }
}

impl<'de, M: 'static> Deserialize<'de> for Cite<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The marker travels with the request so the session can check an
        // arriving citation against the type it registered the id under.
        crate::serde::deserialize_cite(deserializer, TypeId::of::<M>()).map(Cite::new)
    }
}

/// Smart pointer to a registered opaque resource.
///
/// The resource remains valid until every guard is dropped, even if
/// unregistered concurrently.
pub struct OpaqueGuard<T>(Arc<T>);
impl<T> std::ops::Deref for OpaqueGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// A stale opaque handle.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid opaque object")]
pub struct InvalidOpaque;

struct LocalEntry {
    ty: TypeId,
    /// The marker the resource was registered under, checked against the one
    /// the wire position declares when a citation arrives.
    marker: TypeId,
    /// `None` once [`Session::unregister`] has emptied the handle. The entry
    /// itself survives so that a citation still in flight from the peer is
    /// distinguishable from an unknown id, and so the id cannot be reused
    /// while the peer might still name it.
    resource: Option<Arc<dyn Any + Send + Sync>>,
    /// Protocol count: references handed to the peer.
    granted: u32,
    handle: Weak<LocalRef>,
}

impl LocalEntry {
    fn points_at(&self, handle: &LocalRef) -> bool {
        std::ptr::eq(self.handle.as_ptr(), handle as *const LocalRef)
    }
}

struct RemoteEntry {
    /// Protocol count: references the peer has granted for this id. Owned by
    /// the entry rather than by any one handle, so a gift racing the last
    /// handle's drop folds into a single running total.
    granted: u32,
    handle: Weak<RemoteRef>,
}

impl RemoteEntry {
    fn points_at(&self, handle: &RemoteRef) -> bool {
        std::ptr::eq(self.handle.as_ptr(), handle as *const RemoteRef)
    }
}

#[derive(Default)]
struct Tables {
    next: u64,
    local: HashMap<u64, LocalEntry>,
    remote: HashMap<u64, RemoteEntry>,
}

/// One endpoint's half of a session's opaque bookkeeping.
///
/// Both endpoints run the same structure: `local` holds resources this side
/// owns, `remote` mirrors references the peer has granted this side. A client
/// that only ever receives opaques still needs `remote`, because dropping
/// those references is what frees the peer's resources.
pub(crate) struct Session {
    /// Distinguishes this session from every other one in the process, so that
    /// an [`Opaque`] redeemed against the wrong endpoint is caught rather than
    /// silently resolving to whatever that endpoint happens to hold under the
    /// same id.
    serial: u64,
    tables: Mutex<Tables>,
    /// Marker type -> the concrete resource type registered under it, with its
    /// name for diagnostics. Kept out of `tables` so that the conflict panic
    /// cannot poison the table lock.
    markers: Mutex<HashMap<TypeId, (TypeId, &'static str)>>,
    sink: Box<dyn ReleaseSink>,
}

impl Session {
    pub(crate) fn new(sink: Box<dyn ReleaseSink>) -> Arc<Self> {
        static NEXT_SERIAL: AtomicU64 = AtomicU64::new(0);
        Arc::new(Self {
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
            tables: Mutex::new(Tables::default()),
            markers: Mutex::new(HashMap::new()),
            sink,
        })
    }

    /// Rejects an [`Opaque`] minted by a different session.
    ///
    /// A panic rather than an error: ids are session-scoped, so a foreign one
    /// is a pure local logic error that no peer and no race can produce, and
    /// the alternative is resolving it against an unrelated resource that
    /// happens to share the id.
    fn check_serial(&self, serial: u64) {
        assert_eq!(
            serial, self.serial,
            "opaque reference redeemed against a different session"
        );
    }

    /// Records the marker under which `T` is registered, panicking if the
    /// application has already used that marker for a different resource type.
    ///
    /// The wire carries only `(owner, id)`; a marker is what a protocol
    /// declares a position to hold. Two resource types behind one marker would
    /// leave [`take`](Self::take) unable to tell a well-typed citation from a
    /// peer naming the wrong object, so the ambiguity is refused outright.
    fn record_marker<T: OpaqueResource>(&self) {
        let marker = TypeId::of::<T::Marker>();
        let previous = {
            let mut markers = self.markers.lock().unwrap();
            match markers.entry(marker) {
                Entry::Occupied(entry) if entry.get().0 != TypeId::of::<T>() => Some(entry.get().1),
                Entry::Occupied(_) => None,
                Entry::Vacant(entry) => {
                    entry.insert((TypeId::of::<T>(), std::any::type_name::<T>()));
                    None
                }
            }
        };
        // Outside the lock: a panic here must not poison the map for the rest
        // of the session.
        if let Some(previous) = previous {
            panic!(
                "opaque marker `{}` is already registered for resource type `{}`; \
                 it cannot also name `{}`",
                std::any::type_name::<T::Marker>(),
                previous,
                std::any::type_name::<T>(),
            );
        }
    }

    pub(crate) fn register<T: OpaqueResource>(self: &Arc<Self>, value: T) -> Gift<T::Marker> {
        self.record_marker::<T>();
        let mut tables = self.tables.lock().unwrap();
        let id = tables.next;
        tables.next = tables
            .next
            .checked_add(1)
            .expect("opaque identifiers exhausted");
        let handle = Arc::new(LocalRef {
            id,
            serial: self.serial,
            session: Arc::downgrade(self),
        });
        tables.local.insert(
            id,
            LocalEntry {
                ty: TypeId::of::<T>(),
                marker: TypeId::of::<T::Marker>(),
                resource: Some(Arc::new(value)),
                granted: 0,
                handle: Arc::downgrade(&handle),
            },
        );
        Gift::new(Inner::Local(handle))
    }

    pub(crate) fn acquire<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<OpaqueGuard<T>, InvalidOpaque> {
        // Unreachable for a citation that arrived over the wire: a `Cite` is
        // decoded only from `WIRE_CITATION`, which resolves against this
        // endpoint's own table and so is always local. Only a locally minted
        // one can be remote, and `Gift::cite` refuses to mint that.
        let Inner::Local(local) = &value.inner else {
            return Err(InvalidOpaque);
        };
        self.check_serial(local.serial);
        let tables = self.tables.lock().unwrap();
        // Unreachable while this handle is alive: the entry is retired only
        // once no handle points at it, and `cite` refuses an id with no entry
        // rather than minting one.
        let entry = tables.local.get(&local.id).ok_or(InvalidOpaque)?;
        // Likewise unreachable: `cite` rejects a citation whose marker does not
        // match the entry, and `record_marker` makes the marker determine the
        // type. Kept as an integer compare guarding the downcast.
        if entry.ty != TypeId::of::<T>() {
            return Err(InvalidOpaque);
        }
        // The live case: `unregister` emptied the slot while the peer still
        // held a reference, so a citation already in flight lands here.
        let resource = entry.resource.as_ref().ok_or(InvalidOpaque)?;
        Ok(OpaqueGuard(
            resource
                .clone()
                .downcast::<T>()
                .map_err(|_| InvalidOpaque)?,
        ))
    }

    /// Empties the handle, returning the resource if this call held the last
    /// reference to it.
    ///
    /// The registration itself survives until the peer has released every
    /// reference; only the resource slot is cleared. On the `None` path the
    /// resource is *not* restored to the table — outstanding [`OpaqueGuard`]s
    /// keep it alive and it dies with the last one. Restoring it would
    /// resurrect the table's own reference so the resource outlived every
    /// guard, silently turning a close that races an in-flight write into a
    /// no-op; on a pipe's send end that is a missing EOF and a hung reader.
    pub(crate) fn unregister<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<Option<T>, InvalidOpaque> {
        // Unreachable for a decoded citation, as in `acquire`.
        let Inner::Local(local) = &value.inner else {
            return Err(InvalidOpaque);
        };
        self.check_serial(local.serial);
        let mut tables = self.tables.lock().unwrap();
        let entry = tables.local.get_mut(&local.id).ok_or(InvalidOpaque)?;
        if entry.ty != TypeId::of::<T>() {
            return Err(InvalidOpaque);
        }
        let resource = entry.resource.take().ok_or(InvalidOpaque)?;
        let resource = resource.downcast::<T>().map_err(|_| InvalidOpaque)?;
        Ok(Arc::try_unwrap(resource).ok())
    }

    /// Empties the handle, returning the resource if this call held the last
    /// reference to it and *restoring* it if it did not.
    ///
    /// The recovering counterpart of [`unregister`](Self::unregister). On the
    /// `None` path the resource goes back into the table under the same lock
    /// that took it, so nothing observed it missing and the handle the peer
    /// holds keeps working; the caller reports the operation busy without
    /// having destroyed anything.
    ///
    /// That is only sound for an operation which does nothing else on the busy
    /// path. `unregister` deliberately does not restore, because a close that
    /// races an in-flight write must still take effect once the write finishes
    /// — resurrecting the table's reference there would turn the close into a
    /// silent no-op. Use this one where failing is genuinely a no-op, such as a
    /// consuming conversion that the caller may retry.
    pub(crate) fn try_unregister<T: OpaqueResource>(
        &self,
        value: Cite<T::Marker>,
    ) -> Result<Option<T>, InvalidOpaque> {
        // Unreachable for a decoded citation, as in `acquire`.
        let Inner::Local(local) = &value.inner else {
            return Err(InvalidOpaque);
        };
        self.check_serial(local.serial);
        let mut tables = self.tables.lock().unwrap();
        let entry = tables.local.get_mut(&local.id).ok_or(InvalidOpaque)?;
        if entry.ty != TypeId::of::<T>() {
            return Err(InvalidOpaque);
        }
        let resource = entry.resource.take().ok_or(InvalidOpaque)?;
        let resource = match resource.downcast::<T>() {
            Ok(resource) => resource,
            Err(resource) => {
                entry.resource = Some(resource);
                return Err(InvalidOpaque);
            }
        };
        match Arc::try_unwrap(resource) {
            Ok(value) => Ok(Some(value)),
            Err(shared) => {
                entry.resource = Some(shared);
                Ok(None)
            }
        }
    }

    /// Applies a release frame from the peer. Unknown ids are ignored: a
    /// consuming operation races the peer's release by construction.
    pub(crate) fn release(&self, id: u64, count: u32) {
        let mut tables = self.tables.lock().unwrap();
        let Some(entry) = tables.local.get_mut(&id) else {
            return;
        };
        // Saturation deliberately immortalizes the entry for this session.
        // Decrementing it could make a wrapped or otherwise unrepresentable
        // grant total appear finite again.
        if entry.granted != u32::MAX {
            entry.granted = entry.granted.saturating_sub(count);
        }
        if entry.granted == 0 && entry.handle.upgrade().is_none() {
            tables.local.remove(&id);
        }
    }

    /// Records that a gift for `id` is being serialized, and returns the
    /// escrow item holding the reference until the payload is committed.
    fn gift(&self, handle: &Arc<LocalRef>) -> Escrowed {
        let mut tables = self.tables.lock().unwrap();
        if let Some(entry) = tables.local.get_mut(&handle.id) {
            entry.granted = entry.granted.saturating_add(1);
        }
        Escrowed::Gift(handle.clone())
    }

    /// Mirrors an arriving gift for `id`, merging into the handle this
    /// endpoint already holds when there is one.
    fn receive(self: &Arc<Self>, id: u64) -> Inner {
        let (handle, release) = {
            let mut tables = self.tables.lock().unwrap();
            let session = Arc::downgrade(self);
            let entry = tables.remote.entry(id).or_insert_with(|| RemoteEntry {
                granted: 0,
                handle: Weak::new(),
            });
            entry.granted = entry.granted.saturating_add(1);
            let release = if entry.granted >= GRANT_RELEASE_THRESHOLD {
                let release = entry.granted - 1;
                entry.granted = 1;
                Some(release)
            } else {
                None
            };
            // A failed upgrade means the previous handle is mid-`Drop`. It will
            // find the slot no longer pointing at it and leave the running total —
            // including its own references and the one arriving now — to the fresh
            // handle installed here.
            let handle = if let Some(handle) = entry.handle.upgrade() {
                handle
            } else {
                let handle = Arc::new(RemoteRef {
                    id,
                    serial: self.serial,
                    session,
                });
                entry.handle = Arc::downgrade(&handle);
                handle
            };
            (handle, release)
        };
        if let Some(count) = release {
            self.sink.release(id, count);
        }
        Inner::Remote(handle)
    }

    /// Resolves an arriving citation back to a handle on the resource this
    /// endpoint owns.
    ///
    /// Both failures mean the peer has named something it cannot name, and
    /// both are refused rather than papered over.
    ///
    /// An entry registered under a marker other than the one the wire position
    /// declares is the only thing standing between a peer and a guard on the
    /// wrong resource.
    ///
    /// An unknown id means the counts have diverged. It cannot arise from a
    /// race: `granted` rises when a gift is *serialized* and falls only on a
    /// release, the entry is retired only once it reaches zero with no live
    /// handle, and [`Escrowed::Citation`] holds the peer's reference until the
    /// citing payload is fully written — so the release for a cited id is
    /// always generated after the last fragment of the message citing it. A
    /// peer citing a retired id is therefore counting differently than we are,
    /// and nothing it says about this table can be trusted afterwards.
    fn cite(self: &Arc<Self>, id: u64, marker: TypeId) -> Result<Inner, InvalidOpaque> {
        let mut tables = self.tables.lock().unwrap();
        let entry = tables.local.get_mut(&id).ok_or(InvalidOpaque)?;
        if entry.marker != marker {
            return Err(InvalidOpaque);
        }
        if let Some(handle) = entry.handle.upgrade() {
            return Ok(Inner::Local(handle));
        }
        // The owner has dropped its last handle but the peer still holds
        // references, so the entry outlived it. Install a fresh handle.
        let handle = Arc::new(LocalRef {
            id,
            serial: self.serial,
            session: Arc::downgrade(self),
        });
        entry.handle = Arc::downgrade(&handle);
        Ok(Inner::Local(handle))
    }

    /// Resolves an arriving `(owner, id)` pair for a wire position declared to
    /// hold a [`Gift`]: the sender owns the resource and this side mirrors it.
    ///
    /// Nothing is typechecked, because there is nothing here to check against —
    /// the peer's table is the authority on the type of the peer's own
    /// resource. The owner bit is, though: a citation in a gift position names
    /// something this endpoint owns, which is not a reference the peer can
    /// grant.
    pub(crate) fn take_gift(self: &Arc<Self>, owner: u8, id: u64) -> Result<Inner, InvalidOpaque> {
        if owner != WIRE_GIFT {
            return Err(InvalidOpaque);
        }
        Ok(self.receive(id))
    }

    /// Resolves an arriving `(owner, id)` pair for a wire position declared to
    /// hold a [`Cite`] of `marker`: a citation coming home, typechecked as such.
    ///
    /// Both a wrong owner bit and a citation the table cannot account for fail
    /// the decode, which ends the connection — see [`Session::cite`]. Neither
    /// can arise from a race, so tolerating either would mean accepting that
    /// this endpoint and its peer disagree about the table.
    pub(crate) fn take_cite(
        self: &Arc<Self>,
        owner: u8,
        id: u64,
        marker: TypeId,
    ) -> Result<Inner, InvalidOpaque> {
        if owner != WIRE_CITATION {
            return Err(InvalidOpaque);
        }
        self.cite(id, marker)
    }
}

/// A reference held on behalf of a message that is still being written.
enum Escrowed {
    /// A gift whose protocol increment is provisional until the payload is
    /// fully written.
    Gift(Arc<LocalRef>),
    /// A citation. Never read: holding the `Arc` *is* the point, since that
    /// is what keeps the last local handle alive and so orders any resulting
    /// release strictly after the last payload fragment of the message that
    /// cited it. Without it a small release frame could overtake a large body
    /// under round-robin fragmentation, and the peer would retire the entry
    /// before reassembling the message naming it.
    Citation(#[allow(dead_code)] Arc<RemoteRef>),
}

/// The opaque references one outgoing message is holding.
///
/// Serializing moves references in here; the message's terminal outcome
/// decides between [`commit`](Self::commit) and [`rescind`](Self::rescind).
#[derive(Default)]
pub(crate) struct Ledger {
    items: Vec<Escrowed>,
}

impl Ledger {
    /// Records an opaque encountered during serialization, returning its wire
    /// `(owner, id)`.
    pub(crate) fn put(&mut self, value: &Inner, session: &Arc<Session>) -> (u8, u64) {
        match value {
            Inner::Local(local) => {
                // Writing a foreign id onto this session's wire would grant the
                // peer a reference to whatever *this* session holds under that
                // id, so the check matters more here than at redemption.
                session.check_serial(local.serial);
                self.items.push(session.gift(local));
                (WIRE_GIFT, local.id)
            }
            Inner::Remote(remote) => {
                session.check_serial(remote.serial);
                self.items.push(Escrowed::Citation(remote.clone()));
                (WIRE_CITATION, remote.id)
            }
        }
    }

    /// The payload was fully written, so every gift in it is irrevocably
    /// transmitted. Dropping the citations here is what orders any release
    /// they were suppressing after the message that cited them.
    pub(crate) fn commit(self) {
        // Every held reference drops here, on the far side of the payload.
    }

    /// The message was abandoned before its payload completed, so the peer
    /// cannot have decoded it and no gift in it ever landed.
    ///
    /// Only ever correct for an abort that precedes payload completion.
    /// Guessing "delivered" when it was not strands a reference until the
    /// session ends; guessing "not delivered" when it was leaves the peer
    /// holding a freed handle. Leak beats corruption.
    pub(crate) fn rescind(self) {
        for item in &self.items {
            let Escrowed::Gift(handle) = item else {
                continue;
            };
            let Some(session) = handle.session.upgrade() else {
                continue;
            };
            let mut tables = session.tables.lock().unwrap();
            if let Some(entry) = tables.local.get_mut(&handle.id)
                && entry.granted != u32::MAX
            {
                entry.granted = entry.granted.saturating_sub(1);
            }
        }
    }
}

/// Wraps a transport's handle sink with the session context an [`Opaque`]
/// needs, so that serialization has exactly one threaded context rather than
/// two parallel ones.
pub(crate) struct SessionFrame<'a> {
    pub(crate) inner: EncodeHandles,
    pub(crate) session: &'a Arc<Session>,
    pub(crate) ledger: &'a mut Ledger,
}

impl PutHandle for SessionFrame<'_> {
    #[cfg(unix)]
    fn put_handle(&mut self, handle: &dyn ErasedHandle) -> io::Result<u32> {
        self.inner.put_handle(handle)
    }

    #[cfg(windows)]
    fn put_handle(&mut self, handle: &dyn ErasedHandle) -> io::Result<usize> {
        self.inner.put_handle(handle)
    }

    fn put_opaque(&mut self, opaque: &Inner) -> io::Result<(u8, u64)> {
        Ok(self.ledger.put(opaque, self.session))
    }
}

/// The deserialization counterpart of [`SessionFrame`].
#[cfg(unix)]
pub(crate) struct SessionHandles<'a> {
    pub(crate) inner: ReceivedHandles,
    pub(crate) session: &'a Arc<Session>,
}

#[cfg(unix)]
impl TakeHandle for SessionHandles<'_> {
    fn take_handle(&mut self, index: u32) -> io::Result<OwnedFd> {
        self.inner.take_handle(index)
    }

    fn finish(&mut self) -> io::Result<()> {
        self.inner.finish()
    }
    fn take_gift(&mut self, owner: u8, id: u64) -> io::Result<Inner> {
        self.session
            .take_gift(owner, id)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid opaque reference"))
    }

    fn take_cite(&mut self, owner: u8, id: u64, marker: TypeId) -> io::Result<Inner> {
        self.session
            .take_cite(owner, id, marker)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid opaque reference"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, u32)>>);
    impl ReleaseSink for Arc<Recorder> {
        fn release(&self, id: u64, count: u32) {
            self.0.lock().unwrap().push((id, count));
        }
    }

    /// A session whose emitted releases the test can inspect.
    fn session() -> (Arc<Session>, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        (Session::new(Box::new(recorder.clone())), recorder)
    }

    struct Marker;
    struct OtherMarker;
    struct DropMarker;
    struct Value(u32);
    struct OtherValue;
    struct DropValue(Arc<AtomicBool>);
    impl OpaqueResource for Value {
        type Marker = Marker;
    }
    impl OpaqueResource for OtherValue {
        type Marker = OtherMarker;
    }
    impl OpaqueResource for DropValue {
        type Marker = DropMarker;
    }
    impl Drop for DropValue {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// The citation an owner gets back when the peer names one of its
    /// resources to it, which is the only way a `Cite` legitimately reaches
    /// `acquire`.
    fn cited<M: 'static>(session: &Arc<Session>, gift: &Gift<M>) -> Cite<M> {
        Cite::new(
            session
                .take_cite(WIRE_CITATION, gift.inner.id(), TypeId::of::<M>())
                .unwrap(),
        )
    }

    /// A second resource type laying claim to `Value`'s marker.
    struct Impostor;
    impl OpaqueResource for Impostor {
        type Marker = Marker;
    }

    #[test]
    #[should_panic(expected = "is already registered for resource type")]
    fn two_resource_types_under_one_marker_panic_at_registration() {
        let (session, _) = session();
        let _opaque = session.register(Value(42));
        let _conflict = session.register(Impostor);
    }

    #[test]
    fn a_citation_naming_a_differently_typed_entry_is_rejected() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let id = opaque.inner.id();
        assert!(
            session
                .take_cite(WIRE_CITATION, id, TypeId::of::<Marker>())
                .is_ok()
        );
        assert!(
            session
                .take_cite(WIRE_CITATION, id, TypeId::of::<OtherMarker>())
                .is_err()
        );
    }

    #[test]
    #[should_panic(expected = "different session")]
    fn redeeming_an_opaque_against_another_session_panics() {
        let (first, _) = session();
        let (second, _) = session();
        let opaque = first.register(Value(42));
        let _ = second.acquire::<Value>(cited(&first, &opaque));
    }

    #[test]
    #[should_panic(expected = "different session")]
    fn serializing_an_opaque_into_another_session_panics() {
        let (first, _) = session();
        let (second, _) = session();
        let opaque = first.register(Value(42));
        Ledger::default().put(&opaque.inner, &second);
    }

    #[test]
    fn guards_outlive_registration() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let guard = session.acquire::<Value>(cited(&session, &opaque)).unwrap();
        assert!(
            session
                .unregister::<Value>(cited(&session, &opaque))
                .unwrap()
                .is_none()
        );
        assert_eq!(guard.0.0, 42);
        assert!(session.acquire::<Value>(cited(&session, &opaque)).is_err());
    }

    #[test]
    fn unregister_returns_exclusively_owned_value() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let value = session
            .unregister::<Value>(cited(&session, &opaque))
            .unwrap()
            .unwrap();
        assert_eq!(value.0, 42);
    }

    #[test]
    fn try_unregister_restores_a_shared_value() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let guard = session.acquire::<Value>(cited(&session, &opaque)).unwrap();
        assert!(
            session
                .try_unregister::<Value>(cited(&session, &opaque))
                .unwrap()
                .is_none()
        );
        drop(guard);
        // Unlike `unregister`, the handle is still live afterwards, so a retry
        // once the guard is gone succeeds.
        assert_eq!(
            session
                .try_unregister::<Value>(cited(&session, &opaque))
                .unwrap()
                .unwrap()
                .0,
            42
        );
    }

    #[test]
    fn try_unregister_returns_exclusively_owned_value() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let value = session
            .try_unregister::<Value>(cited(&session, &opaque))
            .unwrap()
            .unwrap();
        assert_eq!(value.0, 42);
        assert!(session.acquire::<Value>(cited(&session, &opaque)).is_err());
    }

    #[test]
    fn wrong_type_does_not_remove_value() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let wrong = Cite::<OtherMarker>::new(opaque.inner.clone());
        assert!(session.unregister::<OtherValue>(wrong).is_err());
        assert_eq!(
            session
                .acquire::<Value>(cited(&session, &opaque))
                .unwrap()
                .0
                .0,
            42
        );
    }

    #[test]
    fn dropping_session_drops_registered_values() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (session, _) = session();
        let opaque = session.register(DropValue(dropped.clone()));
        drop(opaque);
        drop(session);
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn dropping_the_last_local_handle_retires_an_ungifted_entry() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        drop(opaque);
        assert!(session.tables.lock().unwrap().local.is_empty());
    }

    #[test]
    fn a_gifted_entry_outlives_its_local_handle_until_released() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let Inner::Local(handle) = &opaque.inner else {
            unreachable!()
        };
        let escrow = session.gift(handle);
        drop(escrow);
        drop(opaque);
        // The peer still holds the reference, so the resource must survive.
        assert_eq!(session.tables.lock().unwrap().local.len(), 1);
        session.release(0, 1);
        assert!(session.tables.lock().unwrap().local.is_empty());
    }

    #[test]
    fn cloning_an_opaque_does_not_grant_a_protocol_reference() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let clones: Vec<_> = (0..8).map(|_| opaque.clone()).collect();
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, 0);
        drop(clones);
        drop(opaque);
        assert!(session.tables.lock().unwrap().local.is_empty());
    }

    #[test]
    fn rescinding_undoes_the_gift_increment() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let mut ledger = Ledger::default();
        assert_eq!(ledger.put(&opaque.inner, &session), (WIRE_GIFT, 0));
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, 1);
        ledger.rescind();
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, 0);
    }

    #[test]
    fn saturated_owner_grant_count_is_immortal() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let Inner::Local(handle) = &opaque.inner else {
            unreachable!()
        };
        session
            .tables
            .lock()
            .unwrap()
            .local
            .get_mut(&0)
            .unwrap()
            .granted = u32::MAX - 1;

        let escrow = session.gift(handle);
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, u32::MAX);
        session.release(0, u32::MAX);
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, u32::MAX);

        let ledger = Ledger {
            items: vec![escrow],
        };
        ledger.rescind();
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, u32::MAX);
        drop(opaque);
        assert!(session.tables.lock().unwrap().local.contains_key(&0));
    }

    #[test]
    fn remote_grants_are_collapsed_at_the_high_threshold() {
        let (session, recorder) = session();
        let first: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 7).unwrap());
        session
            .tables
            .lock()
            .unwrap()
            .remote
            .get_mut(&7)
            .unwrap()
            .granted = GRANT_RELEASE_THRESHOLD - 1;

        let second: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 7).unwrap());
        assert_eq!(first, second);
        assert_eq!(session.tables.lock().unwrap().remote[&7].granted, 1);
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![(7, GRANT_RELEASE_THRESHOLD - 1)]
        );

        drop(first);
        drop(second);
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![(7, GRANT_RELEASE_THRESHOLD - 1), (7, 1)]
        );
    }

    #[test]
    fn committing_leaves_the_gift_increment_in_place() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let mut ledger = Ledger::default();
        ledger.put(&opaque.inner, &session);
        ledger.commit();
        assert_eq!(session.tables.lock().unwrap().local[&0].granted, 1);
    }

    #[test]
    fn citing_an_opaque_has_no_protocol_effect() {
        let (session, recorder) = session();
        let opaque: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 7).unwrap());
        let mut ledger = Ledger::default();
        assert_eq!(
            ledger.put(&opaque.cite().inner, &session),
            (WIRE_CITATION, 7)
        );
        ledger.commit();
        // Still exactly the one reference the gift granted.
        drop(opaque);
        assert_eq!(*recorder.0.lock().unwrap(), vec![(7, 1)]);
    }

    #[test]
    fn repeated_gifts_of_one_id_accumulate_into_a_single_release() {
        let (session, recorder) = session();
        let first: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 3).unwrap());
        let second: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 3).unwrap());
        assert_eq!(first, second);
        drop(first);
        assert!(recorder.0.lock().unwrap().is_empty());
        drop(second);
        assert_eq!(*recorder.0.lock().unwrap(), vec![(3, 2)]);
    }

    /// The owner bit is the peer's to choose, so a wire position declared to
    /// hold one kind must refuse the other where the expectation is still
    /// known — at decode, not at redemption.
    #[test]
    fn a_gift_in_a_citation_position_is_rejected() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let id = opaque.inner.id();
        assert!(
            session
                .take_cite(WIRE_GIFT, id, TypeId::of::<Marker>())
                .is_err()
        );
    }

    #[test]
    fn a_citation_in_a_gift_position_is_rejected() {
        let (session, _) = session();
        assert!(session.take_gift(WIRE_CITATION, 7).is_err());
    }

    /// A citation says "the reference you granted me", so pointing one at
    /// one's own table is a local logic error rather than a protocol event.
    #[test]
    #[should_panic(expected = "cannot cite a resource this endpoint owns")]
    fn citing_a_resource_this_endpoint_owns_panics() {
        let (session, _) = session();
        let _ = session.register(Value(42)).cite();
    }

    #[test]
    #[should_panic(expected = "cannot gift a resource this endpoint does not own")]
    fn gifting_a_resource_the_peer_owns_panics() {
        let (session, _) = session();
        let mirrored: Gift<Marker> = Gift::new(session.take_gift(WIRE_GIFT, 7).unwrap());
        let _ = postcard::to_allocvec(&mirrored);
    }

    /// The mirror image: a citation decoded on the owning side holds a local
    /// handle, and putting it back on the wire would name it to a peer that
    /// never granted it.
    #[test]
    #[should_panic(expected = "cannot cite a resource this endpoint owns")]
    fn re_serializing_a_citation_that_came_home_panics() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let _ = postcard::to_allocvec(&cited(&session, &opaque));
    }

    #[test]
    fn a_citation_for_an_unknown_id_is_rejected() {
        let (session, _) = session();
        assert!(
            session
                .take_cite(WIRE_CITATION, 99, TypeId::of::<Marker>())
                .is_err()
        );
    }

    /// The peer may still be citing a resource the owner has closed, so the
    /// entry has to outlive the resource: emptied is a redemption failure,
    /// absent is a protocol violation.
    #[test]
    fn a_citation_for_an_unregistered_but_still_granted_id_resolves() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        let id = opaque.inner.id();
        // Grant the peer a reference, as sending the opaque would.
        Ledger::default().put(&opaque.inner, &session);
        session
            .unregister::<Value>(cited(&session, &opaque))
            .unwrap();
        let cite = Cite::<Marker>::new(
            session
                .take_cite(WIRE_CITATION, id, TypeId::of::<Marker>())
                .unwrap(),
        );
        assert!(session.acquire::<Value>(cite).is_err());
    }

    #[test]
    fn plain_postcard_use_panics() {
        let (session, _) = session();
        let opaque = session.register(Value(42));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                postcard::to_allocvec(&opaque)
            }))
            .is_err()
        );
        assert!(std::panic::catch_unwind(|| postcard::from_bytes::<Gift<Marker>>(&[0])).is_err());
    }

    #[test]
    fn wire_form_survives_packing_both_owners() {
        for owner in [WIRE_GIFT, WIRE_CITATION] {
            for id in [0, 1, 42, u32::MAX as u64, (1 << 62) - 1] {
                assert_eq!(unpack_wire(pack_wire(owner, id)), (owner, id));
            }
        }
    }

    #[test]
    fn releasing_an_unknown_id_is_ignored() {
        let (session, _) = session();
        session.release(1234, 5);
    }
}
