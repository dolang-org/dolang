//! Typed public API.
//!
//! Everything a consumer of this crate names lives here (plus the plain
//! data types in [`crate::value`] and [`crate::wire`]'s re-exported
//! `PredefinedRoot`/`View`/`Access`). The wire types
//! (`WinRegRequest`/`WinRegResponse`/`WinRegExt`/`KeyMarker`) are an
//! implementation detail, same as `dolang-vfs` never exposing its own
//! `RequestKind`/`ResponseKind`/`VfsProtocol`.

use dolang_vfs::{
    Vfs,
    error::{Error, ErrorKind},
    extension::{ExtCite, ExtGift, VfsExtension},
};
use dolang_winterop::security::SecDesc;
use std::sync::{Arc, Weak};

use crate::{
    value::Value,
    wire::{
        Access, KeyHandle, KeyMarker, LinkTarget, PredefinedRoot, Resolve, View, WinRegExt,
        WinRegRequest, WinRegResponse,
    },
};

const PAGE_SIZE: u32 = 64;

/// A response variant didn't match what the request kind is documented to
/// return.
///
/// Responses can come from a remote peer, so a mismatched variant is invalid
/// wire input rather than a locally provable invariant.
fn unexpected(request: &str) -> Error {
    Error::new(
        ErrorKind::InvalidData,
        format!("unexpected response for {request}"),
    )
}

/// Turns a `WinRegResponse::Key` into a [`Key`], adopting a native handle
/// (see [`KeyHandle`]) into a fresh, purely local [`AnyVfs`] if
/// necessary.
///
/// Adoption pays the cost of one extra in-process `call_extension` — cheap,
/// since `Direct` dispatch never serializes — and only happens once per
/// chain of same-machine keys: every subsequent operation on the returned
/// `Key` (including opening its own subkeys) is dispatched through that
/// a direct `AnyVfs`, which is itself always `native_capable() == false`, so
/// it always takes the ordinary in-process [`ExtGift`] path from then on.
async fn from_response(vfs: &Vfs, request: &str, response: WinRegResponse) -> Result<Key, Error> {
    match response {
        WinRegResponse::Key(KeyHandle::Opaque(handle)) => Ok(Key {
            vfs: vfs.clone(),
            handle: Arc::new(KeyState::new(vfs.clone(), handle)),
        }),
        WinRegResponse::Key(KeyHandle::Native(os_handle)) => {
            let local = Vfs::direct()?;
            let adopted = local
                .call_extension::<WinRegExt>(WinRegRequest::AdoptNative { handle: os_handle })
                .await??;
            match adopted {
                WinRegResponse::Key(KeyHandle::Opaque(handle)) => Ok(Key {
                    vfs: local.clone(),
                    handle: Arc::new(KeyState::new(local, handle)),
                }),
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
    vfs: Vfs,
    handle: Arc<KeyState>,
}

struct KeyState {
    vfs: Vfs,
    opaque: Option<ExtGift<KeyMarker>>,
}

impl KeyState {
    fn new(vfs: Vfs, opaque: ExtGift<KeyMarker>) -> Self {
        Self {
            vfs,
            opaque: Some(opaque),
        }
    }

    fn cite(&self) -> Result<ExtCite<KeyMarker>, Error> {
        self.opaque
            .as_ref()
            .map(ExtGift::cite)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "registry key is closed"))
    }

    fn take_opaque(&mut self) -> ExtGift<KeyMarker> {
        self.opaque.take().expect("live key state has no opaque")
    }
}

impl Drop for KeyState {
    fn drop(&mut self) {
        let Some(key) = self.opaque.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let vfs = self.vfs.clone();
        runtime.spawn(async move {
            let _ = vfs
                .call_extension::<WinRegExt>(WinRegRequest::CloseKey { key: key.cite() })
                .await;
        });
    }
}

impl Key {
    /// Opens a predefined root key (e.g. `HKEY_LOCAL_MACHINE`).
    pub async fn open_root(
        vfs: &Vfs,
        root: PredefinedRoot,
        view: View,
        access: Access,
    ) -> Result<Key, Error> {
        if vfs
            .extensions()
            .maximum_common_version(WinRegExt::NAME, &[WinRegExt::VERSION])
            .is_none()
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "Windows registry is not supported by this VFS backend",
            ));
        }
        let response = vfs
            .call_extension::<WinRegExt>(WinRegRequest::OpenRoot { root, view, access })
            .await??;
        from_response(vfs, "OpenRoot", response).await
    }

    /// Opens an existing subkey of this key.
    pub async fn open(
        &self,
        subpath: &str,
        view: View,
        access: Access,
        resolve: Resolve,
    ) -> Result<Key, Error> {
        let request = match resolve {
            Resolve::Target => WinRegRequest::OpenKey {
                parent: self.handle.cite()?,
                subpath: subpath.to_string(),
                view,
                access,
            },
            Resolve::Link => WinRegRequest::OpenLink {
                parent: self.handle.cite()?,
                subpath: subpath.to_string(),
                view,
                access,
            },
        };
        let response = self.vfs.call_extension::<WinRegExt>(request).await??;
        from_response(&self.vfs, "OpenKey", response).await
    }

    pub async fn link(
        &self,
        target_root: PredefinedRoot,
        target_subpath: &str,
        link_subpath: &str,
        view: View,
    ) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::CreateLink {
                parent: self.handle.cite()?,
                target_root,
                target_subpath: target_subpath.to_string(),
                link_subpath: link_subpath.to_string(),
                view,
            })
            .await??;
        match response {
            WinRegResponse::Ack => Ok(()),
            _ => Err(unexpected("CreateLink")),
        }
    }

    pub async fn read_link(&self, subpath: &str, view: View) -> Result<LinkTarget, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::ReadLink {
                parent: self.handle.cite()?,
                subpath: subpath.to_string(),
                view,
            })
            .await??;
        match response {
            WinRegResponse::LinkTarget(target) => Ok(target),
            _ => Err(unexpected("ReadLink")),
        }
    }

    /// Opens a subkey of this key, creating it (and any missing
    /// intermediate subkeys) if it does not already exist.
    pub async fn create(&self, subpath: &str, view: View, access: Access) -> Result<Key, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::CreateKey {
                parent: self.handle.cite()?,
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
                parent: self.handle.cite()?,
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
        let mut state = Arc::try_unwrap(self.handle).map_err(|handle| {
            drop(handle);
            Error::new(
                ErrorKind::ResourceBusy,
                "registry key has an operation in progress",
            )
        })?;
        let key = state.take_opaque().cite();
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::CloseKey { key })
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
                key: self.handle.cite()?,
                index,
            })
            .await??;
        match response {
            WinRegResponse::Name(name) => Ok(name),
            _ => Err(unexpected("EnumSubkey")),
        }
    }

    /// Opens a forward enumeration of this key's subkeys.
    pub async fn subkeys(&self) -> Result<SubKeys, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::OpenSubkeys {
                key: self.handle.cite()?,
            })
            .await??;
        match response {
            WinRegResponse::EnumerationLen(len) => Ok(SubKeys {
                vfs: self.vfs.clone(),
                key: Arc::downgrade(&self.handle),
                len,
                index: 0,
                entries: std::collections::VecDeque::new(),
                done: false,
            }),
            _ => Err(unexpected("OpenSubkeys")),
        }
    }

    /// Enumerates value names by index. Returns `None` past the last index.
    pub async fn enum_value(&self, index: u32) -> Result<Option<String>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumValue {
                key: self.handle.cite()?,
                index,
            })
            .await??;
        match response {
            WinRegResponse::Name(name) => Ok(name),
            _ => Err(unexpected("EnumValue")),
        }
    }

    /// Opens a forward enumeration of this key's values.
    pub async fn values(&self) -> Result<Values, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::OpenValues {
                key: self.handle.cite()?,
            })
            .await??;
        match response {
            WinRegResponse::EnumerationLen(len) => Ok(Values {
                vfs: self.vfs.clone(),
                key: Arc::downgrade(&self.handle),
                len,
                index: 0,
                entries: std::collections::VecDeque::new(),
                done: false,
            }),
            _ => Err(unexpected("OpenValues")),
        }
    }

    /// Reads a value by name (`None` reads the key's default value).
    pub async fn get_value(&self, name: Option<&str>) -> Result<Option<Value>, Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::GetValue {
                key: self.handle.cite()?,
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
                key: self.handle.cite()?,
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
                key: self.handle.cite()?,
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
                key: self.handle.cite()?,
                mask: dolang_winterop::security::SecInfo::from_bits_retain(mask),
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
    pub async fn update_sec_desc(&self, descriptor: &SecDesc) -> Result<(), Error> {
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::UpdateSecDesc {
                key: self.handle.cite()?,
                sec_desc: descriptor.clone(),
            })
            .await??;
        match response {
            WinRegResponse::Ack => Ok(()),
            _ => Err(unexpected("UpdateSecDesc")),
        }
    }
}

/// A live forward enumeration of registry subkey names.
pub struct SubKeys {
    vfs: Vfs,
    key: Weak<KeyState>,
    len: u32,
    index: u32,
    entries: std::collections::VecDeque<String>,
    done: bool,
}

impl SubKeys {
    /// Returns the subkey count captured when the enumeration was opened.
    pub fn len(&self) -> u32 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Returns the next subkey name.
    pub async fn next_entry(&mut self) -> Result<Option<String>, Error> {
        let key = self
            .key
            .upgrade()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "registry key is closed"))?;
        key.cite()?;
        if let Some(entry) = self.entries.pop_front() {
            return Ok(Some(entry));
        }
        if self.done {
            return Ok(None);
        }
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumSubkeysPage {
                key: key.cite()?,
                index: self.index,
                count: PAGE_SIZE,
            })
            .await??;
        let WinRegResponse::SubkeysPage(entries) = response else {
            return Err(unexpected("EnumSubkeysPage"));
        };
        self.index = self.index.saturating_add(entries.len() as u32);
        self.done = entries.len() < PAGE_SIZE as usize;
        self.entries = entries.into();
        Ok(self.entries.pop_front())
    }
}

/// A live forward enumeration of registry values.
pub struct Values {
    vfs: Vfs,
    key: Weak<KeyState>,
    len: u32,
    index: u32,
    entries: std::collections::VecDeque<(String, Value)>,
    done: bool,
}

impl Values {
    /// Returns the value count captured when the enumeration was opened.
    pub fn len(&self) -> u32 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Returns the next value name and value.
    pub async fn next_entry(&mut self) -> Result<Option<(String, Value)>, Error> {
        let key = self
            .key
            .upgrade()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "registry key is closed"))?;
        key.cite()?;
        if let Some(entry) = self.entries.pop_front() {
            return Ok(Some(entry));
        }
        if self.done {
            return Ok(None);
        }
        let response = self
            .vfs
            .call_extension::<WinRegExt>(WinRegRequest::EnumValuesPage {
                key: key.cite()?,
                index: self.index,
                count: PAGE_SIZE,
            })
            .await??;
        let WinRegResponse::ValuesPage(entries) = response else {
            return Err(unexpected("EnumValuesPage"));
        };
        self.index = self.index.saturating_add(entries.len() as u32);
        self.done = entries.len() < PAGE_SIZE as usize;
        self.entries = entries.into();
        Ok(self.entries.pop_front())
    }
}
