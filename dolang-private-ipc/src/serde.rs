//! This module provides extensions to serde for IPC.
//!
//! This crate uses serde for serialization of data across the process
//! boundary.  Internally this uses [postcard](https://github.com/jamesmunns/postcard)
//! as transfer format.  File handles can also be serialized by using
//! [`Handle`] and [`HandleRef`].
//!
#![cfg_attr(
    feature = "serde-structural",
    doc = r"
Because postcard has various limitations in the structures that can
be serialized, the [`Structural`] wrapper is available which forces
structural serialization (currently uses msgpack).  This requires the
`serde-structural` feature.
"
)]
use std::cell::RefCell;
use std::io;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use serde_::{de, ser};
use serde_::{de::DeserializeOwned, Deserialize, Serialize};

thread_local! {
    // Ser: stack of raw fd accumulator vecs
    static IPC_SER_FDS: RefCell<Vec<Vec<RawFd>>> = const { RefCell::new(Vec::new()) };
    // Deser: stack of Option<OwnedFd> vecs being consumed
    static IPC_DESER_FDS: RefCell<Vec<Vec<Option<OwnedFd>>>> = const { RefCell::new(Vec::new()) };
}

/// Can transfer a unix file handle across processes.
///
/// The basic requirement is that you have an object that implements
/// [`AsFd`] (for serialization) and [`From<OwnedFd>`] (for deserialization).
/// This is the case for regular file objects, sockets, and many more things.
pub struct Handle<F>(F);

/// A raw reference to a handle.
///
/// This serializes the same way as a `Handle` but only uses a raw
/// fd to represent it.  Useful to implement custom serializers.
///
/// Note: `HandleRef` holds a `RawFd` (not `BorrowedFd`) because it must
/// be usable in `Send + 'static` message types.  The caller is responsible
/// for ensuring the fd remains valid for the duration of serialization.
pub struct HandleRef(pub RawFd);

impl<F> Handle<F> {
    /// Wraps the value in a handle.
    pub fn new(f: F) -> Self {
        Handle(f)
    }

    /// Extracts the internal value.
    pub fn into_inner(self) -> F {
        self.0
    }
}

impl<F> From<F> for Handle<F> {
    fn from(f: F) -> Self {
        Handle(f)
    }
}

impl Serialize for HandleRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        if is_ipc_mode() {
            let idx = register_fd(self.0);
            idx.serialize(serializer)
        } else {
            Err(ser::Error::custom("can only serialize in ipc mode"))
        }
    }
}

impl<F: AsFd> Serialize for Handle<F> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        if is_ipc_mode() {
            let raw_fd = self.0.as_fd().as_raw_fd();
            let idx = register_fd(raw_fd);
            idx.serialize(serializer)
        } else {
            Err(ser::Error::custom("can only serialize in ipc mode"))
        }
    }
}

impl<'de, F: From<OwnedFd>> Deserialize<'de> for Handle<F> {
    fn deserialize<D>(deserializer: D) -> Result<Handle<F>, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        if is_ipc_mode() {
            let idx = u32::deserialize(deserializer)?;
            let owned =
                lookup_fd(idx).ok_or_else(|| de::Error::custom("fd not found in mapping"))?;
            Ok(Handle(F::from(owned)))
        } else {
            Err(de::Error::custom("can only deserialize in ipc mode"))
        }
    }
}

struct ResetSerSerde;

impl Drop for ResetSerSerde {
    fn drop(&mut self) {
        IPC_SER_FDS.with(|x| x.borrow_mut().pop());
    }
}

struct ResetDeserSerde;

impl Drop for ResetDeserSerde {
    fn drop(&mut self) {
        IPC_DESER_FDS.with(|x| x.borrow_mut().pop());
    }
}

fn enter_ser_mode<F: FnOnce() -> R, R>(f: F, fds: &mut Vec<RawFd>) -> R {
    IPC_SER_FDS.with(|x| x.borrow_mut().push(Vec::new()));
    let reset = ResetSerSerde;
    let rv = f();
    *fds = IPC_SER_FDS
        .with(|x| x.borrow_mut().pop())
        .unwrap_or_default();
    mem::forget(reset);
    rv
}

fn enter_deser_mode<F: FnOnce() -> R, R>(f: F, fds: Vec<Option<OwnedFd>>) -> R {
    IPC_DESER_FDS.with(|x| x.borrow_mut().push(fds));
    let reset = ResetDeserSerde;
    let rv = f();
    // pop drops the vec, auto-closing any remaining (unused) OwnedFds
    IPC_DESER_FDS.with(|x| x.borrow_mut().pop());
    mem::forget(reset);
    rv
}

fn register_fd(raw_fd: RawFd) -> u32 {
    IPC_SER_FDS.with(|x| {
        let mut x = x.borrow_mut();
        let fds = x.last_mut().unwrap();
        let rv = fds.len() as u32;
        fds.push(raw_fd);
        rv
    })
}

fn lookup_fd(idx: u32) -> Option<OwnedFd> {
    IPC_DESER_FDS.with(|x| {
        let mut x = x.borrow_mut();
        x.last_mut()?.get_mut(idx as usize)?.take()
    })
}

/// Checks if serde is in IPC mode.
///
/// This can be used to customize the behavior of serialization/deserialization
/// implementations for the use with unix-ipc.
pub fn is_ipc_mode() -> bool {
    IPC_SER_FDS.with(|x| !x.borrow().is_empty()) || IPC_DESER_FDS.with(|x| !x.borrow().is_empty())
}

fn postcard_to_io_error(err: postcard::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

/// Serializes something for IPC communication.
///
/// Takes a reference to keep the source value (and any file handles it contains)
/// alive until the caller has finished sending.  The returned `BorrowedFd` slice
/// is tied to the lifetime of `s`: the caller must keep `s` alive until after
/// the fds have been transmitted via `sendmsg`.
///
/// This uses postcard for serialization.  Because UNIX sockets require that
/// file descriptors are transmitted separately they are accumulated in a
/// separate buffer.
pub fn serialize<'a, S: Serialize>(s: &'a S) -> io::Result<(Vec<u8>, Vec<BorrowedFd<'a>>)> {
    let mut raw_fds: Vec<RawFd> = Vec::new();
    let out =
        enter_ser_mode(|| postcard::to_stdvec(s), &mut raw_fds).map_err(postcard_to_io_error)?;
    // SAFETY: BorrowedFd<'a> is #[repr(transparent)] over RawFd.  All raw_fds
    // were obtained via AsFd borrows from *s, so they are valid for 'a.
    let fds = unsafe {
        let mut raw_fds = mem::ManuallyDrop::new(raw_fds);
        Vec::from_raw_parts(
            raw_fds.as_mut_ptr() as *mut BorrowedFd<'a>,
            raw_fds.len(),
            raw_fds.capacity(),
        )
    };
    Ok((out, fds))
}

/// Deserializes something for IPC communication.
///
/// File descriptors need to be provided for deserialization if handles are
/// involved.  Ownership of the fds is transferred; any that are not consumed
/// by deserialization are closed automatically.
pub fn deserialize<D: DeserializeOwned>(bytes: &[u8], fds: Vec<OwnedFd>) -> io::Result<D> {
    let slot: Vec<Option<OwnedFd>> = fds.into_iter().map(Some).collect();
    enter_deser_mode(|| postcard::from_bytes(bytes), slot).map_err(postcard_to_io_error)
}

macro_rules! implement_handle_serialization {
    ($ty:ty) => {
        impl $crate::_serde_ref::Serialize for $ty {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: $crate::_serde_ref::ser::Serializer,
            {
                $crate::_serde_ref::Serialize::serialize(
                    &$crate::serde::HandleRef(self.as_raw_fd()),
                    serializer,
                )
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D>(deserializer: D) -> Result<$ty, D::Error>
            where
                D: $crate::_serde_ref::de::Deserializer<'de>,
            {
                let handle: $crate::serde::Handle<$ty> =
                    $crate::_serde_ref::Deserialize::deserialize(deserializer)?;
                Ok(handle.into_inner())
            }
        }
    };
}

implement_handle_serialization!(crate::RawSender);
implement_handle_serialization!(crate::RawReceiver);

macro_rules! implement_typed_handle_serialization {
    ($ty:ty) => {
        impl<T: Serialize + DeserializeOwned> $crate::_serde_ref::Serialize for $ty {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: $crate::_serde_ref::ser::Serializer,
            {
                $crate::_serde_ref::Serialize::serialize(
                    &$crate::serde::HandleRef(self.as_raw_fd()),
                    serializer,
                )
            }
        }
        impl<'de, T: Serialize + DeserializeOwned> Deserialize<'de> for $ty {
            fn deserialize<D>(deserializer: D) -> Result<$ty, D::Error>
            where
                D: $crate::_serde_ref::de::Deserializer<'de>,
            {
                let handle: $crate::serde::Handle<$ty> =
                    $crate::_serde_ref::Deserialize::deserialize(deserializer)?;
                Ok(handle.into_inner())
            }
        }
    };
}

implement_typed_handle_serialization!(crate::Sender<T>);
implement_typed_handle_serialization!(crate::Receiver<T>);

#[cfg(feature = "serde-structural")]
mod structural {
    use super::*;

    /// Utility wrapper to force values through structural serialization.
    ///
    /// By default `tokio-unix-ipc` uses postcard to serialize data across
    /// process boundaries. This has some limitations which can cause
    /// serialization or deserialization to fail for some types.
    ///
    /// Since the serde ecosystem has some types which require structural
    /// serialization (eg: msgpack, JSON etc.) this type can be used to
    /// work around some known bugs:
    ///
    /// * serde flatten not being supported by compact binary formats in general
    /// * some structural serialization patterns being awkward without an explicit wrapper
    ///
    /// This requires the `serde-structural` feature.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Structural<T>(pub T);

    impl<T: Serialize> Serialize for Structural<T> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: ser::Serializer,
        {
            let msgpack =
                rmp_serde::to_vec(&self.0).map_err(|e| ser::Error::custom(e.to_string()))?;
            serializer.serialize_bytes(&msgpack)
        }
    }

    impl<'de, T: DeserializeOwned> Deserialize<'de> for Structural<T> {
        fn deserialize<D>(deserializer: D) -> Result<Structural<T>, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            let msgpack = Vec::<u8>::deserialize(deserializer)
                .map_err(|e| de::Error::custom(e.to_string()))?;
            Ok(Structural(
                rmp_serde::from_slice(&msgpack).map_err(|e| de::Error::custom(e.to_string()))?,
            ))
        }
    }
}

#[cfg(feature = "serde-structural")]
pub use self::structural::*;

#[test]
fn test_basic() {
    use std::io::Read;
    let f = std::fs::File::open("src/serde.rs").unwrap();
    let handle = Handle::from(f);
    let (bytes, fds) = serialize(&handle).unwrap();
    // Dup each fd manually to simulate what SCM_RIGHTS does across processes
    let owned_fds: Vec<OwnedFd> = fds
        .iter()
        .map(|f| f.try_clone_to_owned().unwrap())
        .collect();
    let f2: Handle<std::fs::File> = deserialize(&bytes, owned_fds).unwrap();
    let mut out = Vec::new();
    f2.into_inner().read_to_end(&mut out).unwrap();
    assert!(out.len() > 100);
}

#[test]
#[cfg(feature = "serde-structural")]
fn test_structural() {
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(crate = "serde_")]
    struct InnerStruct {
        value: u64,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(crate = "serde_")]
    struct BadStruct {
        #[serde(flatten)]
        inner: InnerStruct,
    }

    let (bytes, fds) = serialize(&Structural(BadStruct {
        inner: InnerStruct { value: 42 },
    }))
    .unwrap();
    let owned_fds: Vec<OwnedFd> = fds
        .iter()
        .map(|f| f.try_clone_to_owned().unwrap())
        .collect();
    let value: Structural<BadStruct> = deserialize(&bytes, owned_fds).unwrap();
    assert_eq!(
        value.0,
        BadStruct {
            inner: InnerStruct { value: 42 },
        }
    );
}
