//! Native operating-system handles attached directly to RPC messages.
//!
//! [`OsHandle`] transfers a file descriptor (Unix) or handle (Windows) as a message attachment. It
//! requires a supported transport.

use std::{cell::Cell, fmt, io};

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};

#[cfg(windows)]
use std::os::windows::io::{AsHandle, OwnedHandle};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The platform's default owned native handle type.
#[cfg(unix)]
pub type DefaultHandle = OwnedFd;

/// The platform's default owned native handle type.
#[cfg(windows)]
pub type DefaultHandle = std::os::windows::io::OwnedHandle;

/// Supplies native handles encountered during serialization.
pub(crate) trait PutHandle {
    #[cfg(unix)]
    fn put_handle(&mut self, handle: &dyn ErasedHandle) -> io::Result<u32>;
    #[cfg(windows)]
    fn put_handle(&mut self, handle: &dyn ErasedHandle) -> io::Result<usize>;
    /// Records a session opaque encountered during serialization, returning
    /// its wire `(owner, id)`.
    fn put_opaque(&mut self, opaque: &crate::session::Inner) -> io::Result<(u8, u64)>;
}

pub(crate) trait ErasedHandle {
    #[cfg(unix)]
    fn steal_handle(&self) -> Option<OwnedFd>;
    #[cfg(windows)]
    fn steal_handle(&self) -> Option<OwnedHandle>;
}

/// Consumes native handles encountered during deserialization.
pub(crate) trait TakeHandle {
    #[cfg(unix)]
    fn take_handle(&mut self, index: u32) -> io::Result<OwnedFd>;
    #[cfg(windows)]
    fn take_handle(&mut self, value: usize) -> io::Result<OwnedHandle>;

    fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Resolves an arriving wire `(owner, id)` in a position declared to hold
    /// a [`Gift`](crate::session::Gift) against the receiving session.
    fn take_gift(&mut self, owner: u8, id: u64) -> io::Result<crate::session::Inner>;

    /// Resolves an arriving wire `(owner, id)` in a position declared to hold
    /// a [`Cite`](crate::session::Cite) against the receiving session.
    ///
    /// `marker` is the [`TypeId`](std::any::TypeId) of the marker type the
    /// wire position declares, which the session checks against its own
    /// registration for the id.
    fn take_cite(
        &mut self,
        owner: u8,
        id: u64,
        marker: std::any::TypeId,
    ) -> io::Result<crate::session::Inner>;
}

/// A native operating system handle attachment.
///
/// Only compatible with the [`Builder`](crate::Builder) Unix-socket constructors on Unix or
/// named-pipe constructors on Windows. Serializing it over a generic byte stream fails the call
/// with an error.
pub struct OsHandle<T = DefaultHandle>(Cell<Option<T>>);

impl<T> fmt::Debug for OsHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OsHandle(..)")
    }
}

impl<T> OsHandle<T> {
    /// Wraps a native handle-like value for message attachment.
    pub fn new(value: T) -> Self {
        Self(Cell::new(Some(value)))
    }

    /// Returns the wrapped value.
    ///
    /// # Panics
    ///
    /// Panics if successful serialization already consumed the handle.
    pub fn into_inner(self) -> T {
        self.0
            .into_inner()
            .expect("operating-system handle was already consumed")
    }
}

impl<T> From<T> for OsHandle<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(unix)]
impl<T: AsFd + Into<OwnedFd>> ErasedHandle for OsHandle<T> {
    fn steal_handle(&self) -> Option<OwnedFd> {
        self.0.take().map(Into::into)
    }
}

#[cfg(unix)]
impl<T: AsFd + Into<OwnedFd>> Serialize for OsHandle<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::serde::serialize_handle(self, serializer)
    }
}

#[cfg(unix)]
impl<'de, T: From<OwnedFd>> Deserialize<'de> for OsHandle<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::serde::deserialize_handle(deserializer).map(|handle| OsHandle::new(T::from(handle)))
    }
}

#[cfg(windows)]
impl<T: AsHandle + Into<OwnedHandle>> ErasedHandle for OsHandle<T> {
    fn steal_handle(&self) -> Option<OwnedHandle> {
        self.0.take().map(Into::into)
    }
}

#[cfg(windows)]
impl<T: AsHandle + Into<OwnedHandle>> Serialize for OsHandle<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::serde::serialize_handle(self, serializer)
    }
}

#[cfg(windows)]
impl<'de, T: From<OwnedHandle>> Deserialize<'de> for OsHandle<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::serde::deserialize_handle(deserializer).map(|handle| OsHandle::new(T::from(handle)))
    }
}
