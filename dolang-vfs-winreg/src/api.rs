//! Typed public API.
//!
//! Everything a consumer of this crate names lives here (plus the plain
//! data types in [`crate::value`] and [`crate::wire`]'s re-exported
//! `PredefinedRoot`/`View`/`Access`). The wire types
//! (`WinRegRequest`/`WinRegResponse`/`WinRegExt`/`KeyMarker`) are an
//! implementation detail, same as `dolang-vfs` never exposing its own
//! `RequestKind`/`ResponseKind`/`VfsProtocol`.

use dolang_vfs::{AnyVfs, Direct, Error, ErrorKind, ExtOpaque};
use dolang_winterop::SecDesc;

use crate::{
    value::Value,
    wire::{
        Access, KeyHandle, KeyMarker, PredefinedRoot, View, WinRegExt, WinRegRequest,
        WinRegResponse,
    },
};

/// A response variant didn't match what the request kind is documented to
/// return.
///
/// Responses can come from a remote peer, so a mismatched variant is invalid
/// wire input rather than a locally provable invariant.
fn unexpected(request: &str) -> Error {
    Error::new(
        ErrorKind::Other,
        format!("unexpected response for {request}"),
    )
}

/// Turns a `WinRegResponse::Key` into a [`Key`], adopting a native handle
/// (see [`KeyHandle`]) into a fresh, purely local [`AnyVfs::Direct`] VFS if
/// necessary.
///
/// Adoption pays the cost of one extra in-process `call_extension` — cheap,
/// since `Direct` dispatch never serializes — and only happens once per
/// chain of same-machine keys: every subsequent operation on the returned
/// `Key` (including opening its own subkeys) is dispatched through that
/// `AnyVfs::Direct`, which is itself always `native_capable() == false`, so
/// it always takes the ordinary in-process [`ExtOpaque`] path from then on.
async fn from_response(
    vfs: &AnyVfs,
    request: &str,
    response: WinRegResponse,
) -> Result<Key, Error> {
    match response {
        WinRegResponse::Key(KeyHandle::Opaque(handle)) => Ok(Key {
            vfs: vfs.clone(),
            handle,
        }),
        WinRegResponse::Key(KeyHandle::Native(os_handle)) => {
            let local = AnyVfs::Direct(Direct::default());
            let adopted = local
                .call_extension::<WinRegExt>(WinRegRequest::AdoptNative { handle: os_handle })
                .await??;
            match adopted {
                WinRegResponse::Key(KeyHandle::Opaque(handle)) => Ok(Key { vfs: local, handle }),
                _ => Err(unexpected("AdoptNative")),
            }
        }
        _ => Err(unexpected(request)),
    }
}

/// An open registry key.
///
/// Retains the [`AnyVfs`] it was opened through (`AnyVfs` is cheap to
/// clone — a handle type, not a deep copy), so every operation after the
/// initial [`Key::open_root`] call is dispatched against the same VFS
/// domain that produced the handle. There is no way to pass a `Key` from
/// one `AnyVfs` to an operation routed through a different one, since no
/// method after the bootstrap call accepts a `vfs` argument at all.
pub struct Key {
    vfs: AnyVfs,
    handle: ExtOpaque<KeyMarker>,
}

impl Key {
    /// Opens a predefined root key (e.g. `HKEY_LOCAL_MACHINE`).
    pub async fn open_root(
        vfs: &AnyVfs,
        root: PredefinedRoot,
        view: View,
        access: Access,
    ) -> Result<Key, Error> {
        let response = vfs
            .call_extension::<WinRegExt>(WinRegRequest::OpenRoot { root, view, access })
            .await??;
        from_response(vfs, "OpenRoot", response).await
    }

    /// Opens an existing subkey of this key.
    pub async fn open(&self, subpath: &str, view: View, access: Access) -> Result<Key, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::OpenKey {
                parent: self.handle.clone(),
                subpath: subpath.to_string(),
                view,
                access,
            })
            .await??;
        from_response(&self.vfs, "OpenKey", response).await
    }

    /// Opens a subkey of this key, creating it (and any missing
    /// intermediate subkeys) if it does not already exist.
    pub async fn create(&self, subpath: &str, view: View, access: Access) -> Result<Key, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::CreateKey {
                parent: self.handle.clone(),
                subpath: subpath.to_string(),
                view,
                access,
            })
            .await??;
        from_response(&self.vfs, "CreateKey", response).await
    }

    /// Deletes a subkey of this key, optionally recursively and/or ignoring
    /// a missing subkey.
    pub async fn delete(
        &self,
        subpath: &str,
        view: View,
        all: bool,
        ignore: bool,
    ) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::DeleteKey {
                parent: self.handle.clone(),
                subpath: subpath.to_string(),
                view,
                all,
                ignore,
            })
            .await??;
        match response {
            WinRegResponse::Deleted => Ok(()),
            _ => Err(unexpected("DeleteKey")),
        }
    }

    /// Explicitly closes this key.
    ///
    /// Not required for correctness — an abandoned or dropped `Key` is
    /// closed automatically (in remote mode, when the connection's opaque
    /// object table is torn down) — but lets a well-behaved caller observe
    /// close failures immediately.
    pub async fn close(self) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::CloseKey { key: self.handle })
            .await??;
        match response {
            WinRegResponse::Closed => Ok(()),
            _ => Err(unexpected("CloseKey")),
        }
    }

    /// Enumerates subkey names by index. Returns `None` past the last index.
    pub async fn enum_subkey(&self, index: u32) -> Result<Option<String>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumSubkey {
                key: self.handle.clone(),
                index,
            })
            .await??;
        match response {
            WinRegResponse::Name(name) => Ok(name),
            _ => Err(unexpected("EnumSubkey")),
        }
    }

    /// Fetches every subkey name under this key in one round trip, unlike
    /// calling [`Key::enum_subkey`] for every index.
    pub async fn subkeys(&self) -> Result<Vec<String>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumAllSubkeys {
                key: self.handle.clone(),
            })
            .await??;
        match response {
            WinRegResponse::Subkeys(names) => Ok(names),
            _ => Err(unexpected("EnumAllSubkeys")),
        }
    }

    /// Enumerates value names by index. Returns `None` past the last index.
    pub async fn enum_value(&self, index: u32) -> Result<Option<String>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumValue {
                key: self.handle.clone(),
                index,
            })
            .await??;
        match response {
            WinRegResponse::Name(name) => Ok(name),
            _ => Err(unexpected("EnumValue")),
        }
    }

    /// Fetches every value under this key (name, kind, and data) in one
    /// round trip, unlike calling [`Key::enum_value`] + [`Key::get_value`]
    /// for every index.
    pub async fn values(&self) -> Result<Vec<(String, Value)>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumAllValues {
                key: self.handle.clone(),
            })
            .await??;
        match response {
            WinRegResponse::Values(values) => Ok(values),
            _ => Err(unexpected("EnumAllValues")),
        }
    }

    /// Reads a value by name (`None` reads the key's default value).
    pub async fn get_value(&self, name: Option<&str>) -> Result<Option<Value>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::GetValue {
                key: self.handle.clone(),
                name: name.map(str::to_string),
            })
            .await??;
        match response {
            WinRegResponse::Value(value) => Ok(value.map(|(_, value)| value)),
            _ => Err(unexpected("GetValue")),
        }
    }

    /// Sets a value by name (`None` sets the key's default value).
    pub async fn set_value(&self, name: Option<&str>, value: Value) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::SetValue {
                key: self.handle.clone(),
                name: name.map(str::to_string),
                value,
            })
            .await??;
        match response {
            WinRegResponse::Ack => Ok(()),
            _ => Err(unexpected("SetValue")),
        }
    }

    /// Deletes a value by name (`None` deletes the key's default value).
    pub async fn delete_value(&self, name: Option<&str>) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::DeleteValue {
                key: self.handle.clone(),
                name: name.map(str::to_string),
            })
            .await??;
        match response {
            WinRegResponse::Ack => Ok(()),
            _ => Err(unexpected("DeleteValue")),
        }
    }

    /// Fetches the key's security descriptor. `mask` selects which
    /// components to fetch (an OR of `dolang_vfs`'s
    /// `*_SECURITY_INFORMATION` constants); `0` fetches just the owner.
    ///
    /// Requesting the SACL component requires the key to have been opened
    /// with [`Access::ACCESS_SYSTEM_SECURITY`]. Opening a key with that right
    /// requires `SeSecurityPrivilege`, which is enabled automatically for the
    /// open or create operation if available. Fetching through the resulting
    /// handle does not require the privilege to remain enabled.
    pub async fn sec_desc(&self, mask: u32) -> Result<SecDesc, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::GetSecDesc {
                key: self.handle.clone(),
                mask,
            })
            .await??;
        match response {
            WinRegResponse::SecDesc(descriptor) => Ok(descriptor),
            _ => Err(unexpected("GetSecDesc")),
        }
    }

    /// Sets the key's security descriptor. Which components are updated is
    /// determined by `descriptor.mask()`.
    ///
    /// Setting the DACL/owner/group requires the key to have been opened
    /// with the corresponding `Access::WRITE_DAC`/`WRITE_OWNER` right;
    /// setting the SACL requires [`Access::ACCESS_SYSTEM_SECURITY`] plus
    /// `SeSecurityPrivilege` (elevated automatically for the duration of
    /// this call if available).
    pub async fn set_sec_desc(&self, descriptor: &SecDesc) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::SetSecDesc {
                key: self.handle.clone(),
                sec_desc: descriptor.clone(),
            })
            .await??;
        match response {
            WinRegResponse::Ack => Ok(()),
            _ => Err(unexpected("SetSecDesc")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unexpected_key_response_returns_error() {
        let vfs = AnyVfs::Direct(Direct::default());
        let error = match from_response(&vfs, "OpenRoot", WinRegResponse::Ack).await {
            Ok(_) => panic!("expected an error"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(error.to_string(), "unexpected response for OpenRoot");
    }
}
