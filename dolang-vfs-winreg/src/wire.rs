//! On-the-wire shape of the registry VFS extension.
//!
//! Nothing in this module is exported from the crate root. Callers only ever
//! see [`crate::Key`] and the public data types in [`crate::value`]; the
//! request/response enums here exist solely so [`WinRegExt`] can route and
//! (de)serialize through the VFS extension mechanism.

use dolang_vfs::{Error, ExtContext, ExtOpaque, ExtOsHandle, VfsExtension};
use dolang_winterop::SecDesc;
use serde::{Deserialize, Serialize};

use crate::{backend, value::Value};

/// Marker for the opaque registry key handle. Never named outside this crate.
pub(crate) struct KeyMarker;

/// A key handle returned by an open/create request.
///
/// On a same-machine, native-handle-capable session, the server hands back
/// the raw `HKEY` as an out-of-band [`ExtOsHandle`] attachment instead of
/// registering it in the session's opaque-object table — the caller can
/// then operate on it directly through a local [`dolang_vfs::Direct`]
/// VFS, without any further RPC round trips. See
/// [`WinRegRequest::AdoptNative`] for how a `Native` handle is turned back
/// into an [`ExtOpaque`].
#[derive(Serialize, Deserialize)]
pub(crate) enum KeyHandle {
    Native(ExtOsHandle),
    Opaque(ExtOpaque<KeyMarker>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredefinedRoot {
    ClassesRoot,
    CurrentUser,
    LocalMachine,
    Users,
    CurrentConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    Native,
    Wow32,
    Wow64,
}

/// A Windows access-rights bitmask for opening a key.
///
/// Built by OR-ing named constants together (`Access::READ | Access::WRITE_DAC`),
/// rather than a fixed set of enum variants: unlike file paths, a registry
/// key is opened once and reused for every later operation on it, so a
/// caller that wants (say) to inspect and then modify a key's DACL must be
/// able to request exactly the access rights that requires up front. The
/// values are the stable, documented Win32 SAM desired-access bits, hence
/// no `windows-sys` dependency here — this type stays portable so it still
/// compiles on non-Windows hosts running only the stub backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access(pub u32);

impl Access {
    pub const READ: Access = Access(0x0002_0019); // KEY_READ
    pub const WRITE: Access = Access(0x0002_0006); // KEY_WRITE
    pub const READ_WRITE: Access = Access(Self::READ.0 | Self::WRITE.0);
    pub const READ_CONTROL: Access = Access(0x0002_0000);
    pub const WRITE_DAC: Access = Access(0x0004_0000);
    pub const WRITE_OWNER: Access = Access(0x0008_0000);
    pub const ACCESS_SYSTEM_SECURITY: Access = Access(0x0100_0000);
}

impl std::ops::BitOr for Access {
    type Output = Access;
    fn bitor(self, rhs: Access) -> Access {
        Access(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Access {
    fn bitor_assign(&mut self, rhs: Access) {
        self.0 |= rhs.0;
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinRegRequest {
    OpenRoot {
        root: PredefinedRoot,
        view: View,
        access: Access,
    },
    OpenKey {
        parent: ExtOpaque<KeyMarker>,
        subpath: String,
        view: View,
        access: Access,
    },
    CreateKey {
        parent: ExtOpaque<KeyMarker>,
        subpath: String,
        view: View,
        access: Access,
    },
    CloseKey {
        key: ExtOpaque<KeyMarker>,
    },
    DeleteKey {
        parent: ExtOpaque<KeyMarker>,
        subpath: String,
        view: View,
        all: bool,
        ignore: bool,
    },
    EnumSubkey {
        key: ExtOpaque<KeyMarker>,
        index: u32,
    },
    /// Fetches every subkey name under a key in one round trip, unlike
    /// [`WinRegRequest::EnumSubkey`] which needs one round trip per subkey.
    EnumAllSubkeys {
        key: ExtOpaque<KeyMarker>,
    },
    EnumValue {
        key: ExtOpaque<KeyMarker>,
        index: u32,
    },
    /// Fetches every value under a key (name, kind, and data) in one round
    /// trip, unlike [`WinRegRequest::EnumValue`] + [`WinRegRequest::GetValue`]
    /// which need one round trip per value.
    EnumAllValues {
        key: ExtOpaque<KeyMarker>,
    },
    GetValue {
        key: ExtOpaque<KeyMarker>,
        name: Option<String>,
    },
    SetValue {
        key: ExtOpaque<KeyMarker>,
        name: Option<String>,
        value: Value,
    },
    DeleteValue {
        key: ExtOpaque<KeyMarker>,
        name: Option<String>,
    },
    GetSecDesc {
        key: ExtOpaque<KeyMarker>,
        mask: u32,
    },
    SetSecDesc {
        key: ExtOpaque<KeyMarker>,
        sec_desc: SecDesc,
    },
    /// Adopts a native handle received out-of-band (see [`KeyHandle`]) back
    /// into a registered [`ExtOpaque`].
    ///
    /// Producing an `ExtOpaque` requires an [`ExtContext`], which only
    /// exists inside a `VfsExtension::handle` call — so a client that
    /// receives `KeyHandle::Native` self-dispatches this request against a
    /// local, direct [`dolang_vfs::AnyVfs::Direct`] purely to reach
    /// one. Not exposed outside this crate; used internally by
    /// [`crate::api`].
    AdoptNative {
        handle: ExtOsHandle,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinRegResponse {
    Key(KeyHandle),
    Closed,
    Deleted,
    Name(Option<String>),
    Subkeys(Vec<String>),
    Value(Option<(String, Value)>),
    Values(Vec<(String, Value)>),
    SecDesc(SecDesc),
    Ack,
}

pub(crate) struct WinRegExt;

impl VfsExtension for WinRegExt {
    type Request = WinRegRequest;
    type Response = Result<WinRegResponse, Error>;

    const NAME: &'static str = "dolang-vfs-winreg";
    const VERSION: u16 = 1;

    async fn handle(&self, ctx: &mut ExtContext<'_>, request: WinRegRequest) -> Self::Response {
        backend::handle(ctx, request).await
    }
}

dolang_vfs::vfs_extension!(WinRegExt);
