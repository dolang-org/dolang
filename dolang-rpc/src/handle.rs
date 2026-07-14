use std::{fmt::Formatter, marker::PhantomData, str};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};

#[cfg(unix)]
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
#[cfg(windows)]
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

// This must have a unique address: the serde wrappers use pointer identity to
// ensure only this private newtype can invoke the unsafe handle path.
static OS_HANDLE_TYPE_BYTES: [u8; 20] = *b"dolang_rpc::OsHandle";
pub(crate) static OS_HANDLE_TYPE: &str = match str::from_utf8(&OS_HANDLE_TYPE_BYTES) {
    Ok(value) => value,
    Err(_) => panic!("invalid handle type marker"),
};

/// The platform's default owned native handle type.
#[cfg(unix)]
pub type DefaultHandle = OwnedFd;

/// The platform's default owned native handle type.
#[cfg(windows)]
pub type DefaultHandle = std::os::windows::io::OwnedHandle;

/// A native operating-system resource transferred as a frame attachment.
///
/// Direct attachment serialization is available only through an
/// attachment-capable session transport.
#[cfg(any(unix, windows))]
pub struct OsHandle<T = DefaultHandle>(T);

#[cfg(not(any(unix, windows)))]
pub struct OsHandle<T>(T);

impl<T> OsHandle<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
    pub fn into_inner(self) -> T {
        self.0
    }
    pub fn as_inner(&self) -> &T {
        &self.0
    }
}

impl<T> From<T> for OsHandle<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

#[cfg(unix)]
impl<T: AsFd> Serialize for OsHandle<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_newtype_struct(OS_HANDLE_TYPE, &self.0.as_fd().as_raw_fd())
    }
}

#[cfg(unix)]
impl<'de, T: From<OwnedFd>> Deserialize<'de> for OsHandle<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);

        impl<'de, T: From<OwnedFd>> de::Visitor<'de> for Visitor<T> {
            type Value = OsHandle<T>;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an operating-system handle")
            }

            fn visit_newtype_struct<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                use std::os::fd::FromRawFd;
                let raw = i32::deserialize(deserializer)?;
                Ok(OsHandle(T::from(unsafe { OwnedFd::from_raw_fd(raw) })))
            }
        }

        deserializer.deserialize_newtype_struct(OS_HANDLE_TYPE, Visitor(PhantomData))
    }
}

#[cfg(windows)]
impl<T: AsHandle> Serialize for OsHandle<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let raw = self.0.as_handle().as_raw_handle() as usize;
        #[cfg(target_pointer_width = "32")]
        return serializer.serialize_newtype_struct(OS_HANDLE_TYPE, &(raw as u32));
        #[cfg(target_pointer_width = "64")]
        return serializer.serialize_newtype_struct(OS_HANDLE_TYPE, &(raw as u64));
    }
}

#[cfg(windows)]
impl<'de, T: From<OwnedHandle>> Deserialize<'de> for OsHandle<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);

        impl<'de, T: From<OwnedHandle>> de::Visitor<'de> for Visitor<T> {
            type Value = OsHandle<T>;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an operating-system handle")
            }

            fn visit_newtype_struct<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                #[cfg(target_pointer_width = "32")]
                let raw = u32::deserialize(deserializer)? as usize;
                #[cfg(target_pointer_width = "64")]
                let raw = u64::deserialize(deserializer)? as usize;
                Ok(OsHandle(T::from(unsafe {
                    OwnedHandle::from_raw_handle(raw as _)
                })))
            }
        }

        deserializer.deserialize_newtype_struct(OS_HANDLE_TYPE, Visitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SAME_TYPE_BYTES: [u8; 20] = *b"dolang_rpc::OsHandle";

    #[test]
    fn handle_type_uses_identity_not_contents() {
        let same = unsafe { std::str::from_utf8_unchecked(&SAME_TYPE_BYTES) };
        assert_eq!(same, OS_HANDLE_TYPE);
        assert!(!std::ptr::eq(same, OS_HANDLE_TYPE));
    }
}
