use std::any::Any;
use std::collections::HashMap;
use std::{io, path::PathBuf};

use dolang_rpc::{Opaque, OsHandle, Protocol};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, SeqAccess, Visitor},
    ser::SerializeTuple,
};

use crate::extension::ErasedVfsExtension;
pub(crate) use crate::{
    DirEntry, FsMetadata, Metadata, MetadataPatch, PosixAcl, SecurityInfo, SidName, StreamEntry,
    TargetInfo, WellKnownPath, XattrEntry, XattrNamespace,
};
pub(crate) use dolang_winterop::{SecDesc, Sid};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(transparent)]
pub(crate) struct WireError(crate::Error);

impl From<crate::Error> for WireError {
    fn from(error: crate::Error) -> Self {
        Self(error)
    }
}

impl From<WireError> for crate::Error {
    fn from(error: WireError) -> Self {
        error.0
    }
}

pub(crate) struct VfsProtocol;

impl Protocol for VfsProtocol {
    type Request = Request;
    type Response = ResponseKind;
}

/// Application-protocol name/version advertised during the RPC handshake.
/// `dolang_rpc::Server`/`Client` are only reachable via `UnboundServer`/
/// `UnboundClient`, which require this descriptor.
pub(crate) const APP_PROTOCOL: (&str, &[u16]) = ("dolang-vfs", &[1]);

/// Starts a fresh [`dolang_rpc::Builder`] preconfigured with
/// [`APP_PROTOCOL`].
pub(crate) fn rpc_builder() -> dolang_rpc::Builder {
    dolang_rpc::Builder::new(APP_PROTOCOL.0, APP_PROTOCOL.1)
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Request {
    pub(crate) vfs: Option<Opaque<crate::VfsMarker>>,
    pub(crate) kind: RequestKind,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct QueryResponse {
    pub(crate) env: HashMap<String, String>,
    pub(crate) cwd: WirePath,
    pub(crate) current_exe: WirePath,
    pub(crate) target: TargetInfo,
    pub(crate) security: SecurityInfo,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WirePathKind {
    Unix,
    Windows,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct WirePath {
    kind: WirePathKind,
    path: String,
}

impl WirePath {
    pub(crate) fn empty_like(path: crate::Utf8TypedPath<'_>) -> Self {
        Self {
            kind: match path {
                crate::Utf8TypedPath::Unix(_) => WirePathKind::Unix,
                crate::Utf8TypedPath::Windows(_) => WirePathKind::Windows,
            },
            path: String::new(),
        }
    }
}

impl From<crate::Utf8TypedPath<'_>> for WirePath {
    fn from(path: crate::Utf8TypedPath<'_>) -> Self {
        match path {
            crate::Utf8TypedPath::Unix(path) => Self {
                kind: WirePathKind::Unix,
                path: path.as_str().to_owned(),
            },
            crate::Utf8TypedPath::Windows(path) => Self {
                kind: WirePathKind::Windows,
                path: path.as_str().to_owned(),
            },
        }
    }
}

impl From<crate::Utf8TypedPathBuf> for WirePath {
    fn from(path: crate::Utf8TypedPathBuf) -> Self {
        path.to_path().into()
    }
}

impl<'a> From<&'a WirePath> for crate::Utf8TypedPath<'a> {
    fn from(path: &'a WirePath) -> Self {
        match path.kind {
            WirePathKind::Unix => crate::Utf8TypedPath::Unix(crate::Utf8UnixPath::new(&path.path)),
            WirePathKind::Windows => {
                crate::Utf8TypedPath::Windows(crate::Utf8WindowsPath::new(&path.path))
            }
        }
    }
}

impl From<WirePath> for crate::Utf8TypedPathBuf {
    fn from(path: WirePath) -> Self {
        match path.kind {
            WirePathKind::Unix => crate::Utf8TypedPathBuf::from_unix(path.path),
            WirePathKind::Windows => crate::Utf8TypedPathBuf::from_windows(path.path),
        }
    }
}

impl TryFrom<PathBuf> for WirePath {
    type Error = crate::Error;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        crate::typed_path(path).map(Into::into).map_err(Into::into)
    }
}

impl TryFrom<WirePath> for PathBuf {
    type Error = crate::Error;

    fn try_from(path: WirePath) -> Result<Self, Self::Error> {
        crate::native_path(crate::Utf8TypedPathBuf::from(path).to_path()).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{WireError, WirePath, WirePathKind};
    use crate::{
        Error, ErrorKind, OperatingSystem, Utf8TypedPath, Utf8TypedPathBuf, Utf8UnixPath,
        Utf8WindowsPath,
    };

    #[test]
    fn wire_path_preserves_unix_kind_and_literal_form() {
        let wire = WirePath::from(Utf8TypedPath::Unix(Utf8UnixPath::new(r"foo\bar/baz")));
        assert_eq!(wire.kind, WirePathKind::Unix);
        assert_eq!(wire.path, r"foo\bar/baz");

        let borrowed = Utf8TypedPath::from(&wire);
        assert!(matches!(borrowed, Utf8TypedPath::Unix(_)));
        assert_eq!(borrowed.as_str(), r"foo\bar/baz");

        let owned = Utf8TypedPathBuf::from(wire);
        assert!(matches!(owned, Utf8TypedPathBuf::Unix(_)));
        assert_eq!(owned.as_str(), r"foo\bar/baz");
    }

    #[test]
    fn wire_path_preserves_windows_kind_and_literal_form() {
        let wire = WirePath::from(Utf8TypedPath::Windows(Utf8WindowsPath::new(r"C:\foo/bar")));
        assert_eq!(wire.kind, WirePathKind::Windows);
        assert_eq!(wire.path, r"C:\foo/bar");

        let borrowed = Utf8TypedPath::from(&wire);
        assert!(matches!(borrowed, Utf8TypedPath::Windows(_)));
        assert_eq!(borrowed.as_str(), r"C:\foo/bar");

        let owned = Utf8TypedPathBuf::from(wire);
        assert!(matches!(owned, Utf8TypedPathBuf::Windows(_)));
        assert_eq!(owned.as_str(), r"C:\foo/bar");
    }

    #[test]
    fn native_conversion_rejects_the_other_path_kind() {
        let wire = if cfg!(windows) {
            WirePath::from(Utf8TypedPath::Unix(Utf8UnixPath::new("foo")))
        } else {
            WirePath::from(Utf8TypedPath::Windows(Utf8WindowsPath::new("foo")))
        };
        assert!(PathBuf::try_from(wire).is_err());
    }

    #[test]
    fn wire_error_preserves_foreign_system_error() {
        let error = Error::from_system_code(
            ErrorKind::PermissionDenied,
            "access is denied",
            OperatingSystem::Windows,
            5,
        );

        let error = Error::from(WireError::from(error));
        let system = error.system_code().unwrap();
        assert_eq!(system.operating_system(), OperatingSystem::Windows);
        assert_eq!(system.raw(), 5);
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(error.message(), "access is denied");
    }

    #[test]
    fn wire_error_preserves_incidental_io_error() {
        let error = Error::new(ErrorKind::InvalidData, "bad reply");

        let error = Error::from(WireError::from(error));
        assert!(error.system_code().is_none());
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "bad reply");
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum XattrNamespaceRequest {
    Default,
    Named(String),
    Any,
}

impl From<XattrNamespace<'_>> for XattrNamespaceRequest {
    fn from(value: XattrNamespace<'_>) -> Self {
        match value {
            XattrNamespace::Default => Self::Default,
            XattrNamespace::Named(namespace) => Self::Named(namespace.to_owned()),
            XattrNamespace::Any => Self::Any,
        }
    }
}

impl XattrNamespaceRequest {
    pub(crate) fn as_borrowed(&self) -> XattrNamespace<'_> {
        match self {
            Self::Default => XattrNamespace::Default,
            Self::Named(namespace) => XattrNamespace::Named(namespace),
            Self::Any => XattrNamespace::Any,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SpawnRequest {
    pub(crate) program: WirePath,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, Option<String>>,
    pub(crate) cwd: Option<WirePath>,
    pub(crate) stdin: StdioRecvTarget,
    pub(crate) stdout: StdioSendTarget,
    pub(crate) stderr: StdioSendTarget,
    pub(crate) process_control: crate::ProcessControl,
    pub(crate) termination_policy: crate::TerminationPolicy,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct PipeResponse {
    pub(crate) send: Opaque<crate::StdioSendMarker>,
    pub(crate) recv: Opaque<crate::StdioRecvMarker>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum StdioRecvTarget {
    Null,
    Native(OsHandle),
    Opaque(Opaque<crate::StdioRecvMarker>),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum StdioSendTarget {
    Null,
    Native(OsHandle),
    Opaque(Opaque<crate::StdioSendMarker>),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct OpenRequest {
    pub(crate) path: WirePath,
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) append: bool,
    pub(crate) create: bool,
    pub(crate) create_new: bool,
    pub(crate) truncate: bool,
    pub(crate) no_follow: bool,
    pub(crate) handle_preference: OpenHandlePreference,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ReadDirResponse {
    pub(crate) entries: Vec<DirEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) enum OpenHandlePreference {
    NativePreferred,
    Opaque,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum OpenHandle {
    Native(OsHandle),
    Opaque(Opaque<crate::FileMarker>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) enum FileSeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

impl From<io::SeekFrom> for FileSeekFrom {
    fn from(value: io::SeekFrom) -> Self {
        match value {
            io::SeekFrom::Start(offset) => Self::Start(offset),
            io::SeekFrom::End(offset) => Self::End(offset),
            io::SeekFrom::Current(offset) => Self::Current(offset),
        }
    }
}

impl From<FileSeekFrom> for io::SeekFrom {
    fn from(value: FileSeekFrom) -> Self {
        match value {
            FileSeekFrom::Start(offset) => Self::Start(offset),
            FileSeekFrom::End(offset) => Self::End(offset),
            FileSeekFrom::Current(offset) => Self::Current(offset),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct UnixVfsRequest {
    pub(crate) path: WirePath,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WindowsAdminRequest {
    pub(crate) cwd: WirePath,
    pub(crate) env: HashMap<String, Option<String>>,
    pub(crate) elevate: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum OpenVfsHandle {
    Native(OsHandle),
    Opaque(Opaque<crate::VfsMarker>),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveRequest {
    pub(crate) path: WirePath,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CreateDirRequest {
    pub(crate) path: WirePath,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveDirRequest {
    pub(crate) path: WirePath,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MetadataRequest {
    pub(crate) path: WirePath,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct FsMetadataRequest {
    pub(crate) path: WirePath,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AclRequest {
    pub(crate) path: WirePath,
    pub(crate) default: bool,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetAclRequest {
    pub(crate) path: WirePath,
    pub(crate) acl: Option<PosixAcl>,
    pub(crate) default: bool,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SecDescRequest {
    pub(crate) path: WirePath,
    pub(crate) mask: u32,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetSecDescRequest {
    pub(crate) path: WirePath,
    pub(crate) sec_desc: SecDesc,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetMetadataRequest {
    pub(crate) paths: Vec<WirePath>,
    pub(crate) patch: MetadataPatch,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CopyRequest {
    pub(crate) from: WirePath,
    pub(crate) to: WirePath,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RenameRequest {
    pub(crate) from: WirePath,
    pub(crate) to: WirePath,
    pub(crate) replace: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MoveRequest {
    pub(crate) from: WirePath,
    pub(crate) to: WirePath,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum SymlinkKind {
    Infer,
    Dir,
    File,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SymlinkRequest {
    pub(crate) cwd: WirePath,
    pub(crate) src: WirePath,
    pub(crate) dst: WirePath,
    pub(crate) kind: SymlinkKind,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct HardLinkRequest {
    pub(crate) src: WirePath,
    pub(crate) dst: WirePath,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CanonicalizeRequest {
    pub(crate) path: WirePath,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ReadLinkRequest {
    pub(crate) path: WirePath,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AccessRequest {
    pub(crate) path: WirePath,
    pub(crate) mode: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct GlobRequest {
    pub(crate) pattern: String,
    pub(crate) root: WirePath,
    pub(crate) follow_symlinks: bool,
    pub(crate) max_depth: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WellKnownPathRequest {
    pub(crate) key: WellKnownPath,
    pub(crate) app: Option<String>,
    pub(crate) env: HashMap<String, Option<String>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct XattrsRequest {
    pub(crate) path: WirePath,
    pub(crate) namespace: XattrNamespaceRequest,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct StreamsRequest {
    pub(crate) path: WirePath,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct XattrRequest {
    pub(crate) path: WirePath,
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetXattrRequest {
    pub(crate) path: WirePath,
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
    pub(crate) value: Vec<u8>,
    pub(crate) follow: bool,
}

/// Wire envelope for a VFS extension request.
///
/// `name`/`version` route to the extension via `crate::extension::lookup`;
/// `payload` is the extension's own request type, boxed and type-erased.
/// The `Serialize`/`Deserialize` impls below are manual (not derived)
/// because the concrete payload type is only known once `name`/`version`
/// have been read and looked up, so deserialization uses a
/// `DeserializeSeed` to defer to the extension's own `Request` type mid-way
/// through decoding the same tuple — see `dolang-vfs/ARCHITECTURE.md`
/// for the full rationale.
pub(crate) struct ExtensionRequest {
    pub(crate) name: String,
    pub(crate) version: u16,
    pub(crate) payload: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for ExtensionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRequest")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Serialize for ExtensionRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ext = crate::extension::lookup(&self.name, self.version)
            .ok_or_else(|| serde::ser::Error::custom("unknown VFS extension"))?;
        let mut tup = serializer.serialize_tuple(3)?;
        tup.serialize_element(&self.name)?;
        tup.serialize_element(&self.version)?;
        tup.serialize_element(ext.erase_request(&*self.payload))?;
        tup.end()
    }
}

struct ExtensionRequestSeed(&'static dyn ErasedVfsExtension);

impl<'de> serde::de::DeserializeSeed<'de> for ExtensionRequestSeed {
    type Value = Box<dyn Any + Send + Sync>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let mut erased = <dyn erased_serde::Deserializer>::erase(deserializer);
        self.0
            .deserialize_request(&mut erased)
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for ExtensionRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ExtensionRequest;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a VFS extension request")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let name: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let version: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                let ext = crate::extension::lookup(&name, version)
                    .ok_or_else(|| A::Error::custom("unknown VFS extension"))?;
                let payload = seq
                    .next_element_seed(ExtensionRequestSeed(ext))?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;
                Ok(ExtensionRequest {
                    name,
                    version,
                    payload,
                })
            }
        }
        deserializer.deserialize_tuple(3, V)
    }
}

/// Wire envelope for a VFS extension response. See [`ExtensionRequest`] for
/// why `Serialize`/`Deserialize` are implemented manually. `name`/`version`
/// are echoed back from the request so the client, which decodes
/// `ResponseKind` generically inside `dolang_rpc` before it ever reaches the
/// extension-typed call site, can still find the matching extension to
/// decode `payload` as that extension's `Response` type.
pub(crate) struct ExtensionResponse {
    pub(crate) name: String,
    pub(crate) version: u16,
    pub(crate) payload: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for ExtensionResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionResponse")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Serialize for ExtensionResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ext = crate::extension::lookup(&self.name, self.version)
            .ok_or_else(|| serde::ser::Error::custom("unknown VFS extension"))?;
        let mut tup = serializer.serialize_tuple(3)?;
        tup.serialize_element(&self.name)?;
        tup.serialize_element(&self.version)?;
        tup.serialize_element(ext.erase_response(&*self.payload))?;
        tup.end()
    }
}

struct ExtensionResponseSeed(&'static dyn ErasedVfsExtension);

impl<'de> serde::de::DeserializeSeed<'de> for ExtensionResponseSeed {
    type Value = Box<dyn Any + Send + Sync>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let mut erased = <dyn erased_serde::Deserializer>::erase(deserializer);
        self.0
            .deserialize_response(&mut erased)
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for ExtensionResponse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ExtensionResponse;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a VFS extension response")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let name: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let version: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                let ext = crate::extension::lookup(&name, version)
                    .ok_or_else(|| A::Error::custom("unknown VFS extension"))?;
                let payload = seq
                    .next_element_seed(ExtensionResponseSeed(ext))?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;
                Ok(ExtensionResponse {
                    name,
                    version,
                    payload,
                })
            }
        }
        deserializer.deserialize_tuple(3, V)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum RequestKind {
    Spawn(SpawnRequest),
    ChildWait {
        child: Opaque<crate::ChildMarker>,
    },
    ChildTerminate {
        child: Opaque<crate::ChildMarker>,
    },
    ChildClose {
        child: Opaque<crate::ChildMarker>,
    },
    Query,
    UserName {
        uid: u32,
    },
    UserId {
        name: String,
    },
    GroupName {
        gid: u32,
    },
    GroupId {
        name: String,
    },
    SidName {
        sid: Sid,
    },
    AccountName {
        name: String,
    },
    Which {
        program: WirePath,
        path: Option<String>,
        cwd: Option<WirePath>,
    },
    WellKnownPath(WellKnownPathRequest),
    Stop,
    ClearCache,
    Pipe,
    Open(OpenRequest),
    FileRead {
        file: Opaque<crate::FileMarker>,
        len: usize,
    },
    FileWrite {
        file: Opaque<crate::FileMarker>,
    },
    FileSeek {
        file: Opaque<crate::FileMarker>,
        position: FileSeekFrom,
    },
    FileFlush {
        file: Opaque<crate::FileMarker>,
    },
    FileSetSize {
        file: Opaque<crate::FileMarker>,
        size: u64,
    },
    FileLock {
        file: Opaque<crate::FileMarker>,
        request: crate::FileLockRequest,
    },
    FileUnlock {
        file: Opaque<crate::FileMarker>,
        lock: u64,
    },
    FileToStdioSend {
        file: Opaque<crate::FileMarker>,
    },
    FileToStdioRecv {
        file: Opaque<crate::FileMarker>,
    },
    StdioSendClose {
        stdio: Opaque<crate::StdioSendMarker>,
    },
    StdioSendWrite {
        stdio: Opaque<crate::StdioSendMarker>,
    },
    StdioSendClone {
        stdio: Opaque<crate::StdioSendMarker>,
    },
    StdioRecvClose {
        stdio: Opaque<crate::StdioRecvMarker>,
    },
    StdioRecvRead {
        stdio: Opaque<crate::StdioRecvMarker>,
        len: usize,
    },
    StdioRecvClone {
        stdio: Opaque<crate::StdioRecvMarker>,
    },
    FileMetadata {
        file: Opaque<crate::FileMarker>,
    },
    FileFsMetadata {
        file: Opaque<crate::FileMarker>,
    },
    FileAcl {
        file: Opaque<crate::FileMarker>,
        default: bool,
    },
    FileSetAcl {
        file: Opaque<crate::FileMarker>,
        acl: Option<PosixAcl>,
        default: bool,
    },
    FileSecDesc {
        file: Opaque<crate::FileMarker>,
        mask: u32,
    },
    FileSetSecDesc {
        file: Opaque<crate::FileMarker>,
        sec_desc: SecDesc,
    },
    FileXattrs {
        file: Opaque<crate::FileMarker>,
        namespace: XattrNamespaceRequest,
    },
    FileXattr {
        file: Opaque<crate::FileMarker>,
        name: String,
        namespace: Option<String>,
    },
    FileStreams {
        file: Opaque<crate::FileMarker>,
    },
    FileSetXattr {
        file: Opaque<crate::FileMarker>,
        name: String,
        namespace: Option<String>,
        value: Vec<u8>,
    },
    FileRemoveXattr {
        file: Opaque<crate::FileMarker>,
        name: String,
        namespace: Option<String>,
    },
    FileClose {
        file: Opaque<crate::FileMarker>,
    },
    UnixVfs(UnixVfsRequest),
    WindowsAdmin(WindowsAdminRequest),
    ReadDir {
        path: WirePath,
    },
    Remove(RemoveRequest),
    Metadata(MetadataRequest),
    FsMetadata(FsMetadataRequest),
    Acl(AclRequest),
    SetAcl(SetAclRequest),
    SecDesc(SecDescRequest),
    SetSecDesc(SetSecDescRequest),
    CreateDir(CreateDirRequest),
    RemoveDir(RemoveDirRequest),
    Copy(CopyRequest),
    Rename(RenameRequest),
    Move(MoveRequest),
    Symlink(SymlinkRequest),
    HardLink(HardLinkRequest),
    SymlinkMetadata(MetadataRequest),
    SetMetadata(SetMetadataRequest),
    Canonicalize(CanonicalizeRequest),
    ReadLink(ReadLinkRequest),
    Access(AccessRequest),
    Glob(GlobRequest),
    Xattrs(XattrsRequest),
    Xattr(XattrRequest),
    SetXattr(SetXattrRequest),
    RemoveXattr(XattrRequest),
    Streams(StreamsRequest),
    Extension(ExtensionRequest),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum ResponseKind {
    Error(WireError),
    Spawn(Result<Opaque<crate::ChildMarker>, WireError>),
    ChildWait(Result<crate::ProcessStatus, WireError>),
    ChildTerminate(Result<Option<crate::ProcessStatus>, WireError>),
    ChildClose(Result<(), WireError>),
    Query(Result<QueryResponse, WireError>),
    UserName(Result<String, WireError>),
    UserId(Result<u32, WireError>),
    GroupName(Result<String, WireError>),
    GroupId(Result<u32, WireError>),
    SidName(Result<SidName, WireError>),
    AccountName(Result<SidName, WireError>),
    Which(Result<Option<WirePath>, WireError>),
    WellKnownPath(Result<WirePath, WireError>),
    Stop,
    ClearCache(Result<(), WireError>),
    Pipe(Result<PipeResponse, WireError>),
    Open(Result<OpenHandle, WireError>),
    FileRead(Result<(), WireError>),
    FileWrite(Result<usize, WireError>),
    FileSeek(Result<u64, WireError>),
    FileFlush(Result<(), WireError>),
    FileSetSize(Result<(), WireError>),
    FileLock(Result<Option<u64>, WireError>),
    FileUnlock(Result<(), WireError>),
    FileToStdioSend(Result<Opaque<crate::StdioSendMarker>, WireError>),
    FileToStdioRecv(Result<Opaque<crate::StdioRecvMarker>, WireError>),
    StdioSendClose(Result<(), WireError>),
    StdioSendWrite(Result<usize, WireError>),
    StdioSendClone(Result<Opaque<crate::StdioSendMarker>, WireError>),
    StdioRecvClose(Result<(), WireError>),
    StdioRecvRead(Result<(), WireError>),
    StdioRecvClone(Result<Opaque<crate::StdioRecvMarker>, WireError>),
    FileMetadata(Result<Metadata, WireError>),
    FileFsMetadata(Result<FsMetadata, WireError>),
    FileAcl(Result<Option<PosixAcl>, WireError>),
    FileSetAcl(Result<(), WireError>),
    FileSecDesc(Result<SecDesc, WireError>),
    FileSetSecDesc(Result<(), WireError>),
    FileXattrs(Result<Vec<XattrEntry>, WireError>),
    FileXattr(Result<Vec<u8>, WireError>),
    FileStreams(Result<Vec<StreamEntry>, WireError>),
    FileSetXattr(Result<(), WireError>),
    FileRemoveXattr(Result<(), WireError>),
    FileClose(Result<(), WireError>),
    UnixVfs(Result<OpenVfsHandle, WireError>),
    WindowsAdmin(Result<Opaque<crate::VfsMarker>, WireError>),
    ReadDir(Result<ReadDirResponse, WireError>),
    Remove(Result<(), WireError>),
    Metadata(Result<Metadata, WireError>),
    FsMetadata(Result<FsMetadata, WireError>),
    Acl(Result<Option<PosixAcl>, WireError>),
    SetAcl(Result<(), WireError>),
    SecDesc(Result<SecDesc, WireError>),
    SetSecDesc(Result<(), WireError>),
    CreateDir(Result<(), WireError>),
    RemoveDir(Result<(), WireError>),
    Copy(Result<(), WireError>),
    Rename(Result<(), WireError>),
    Move(Result<(), WireError>),
    Symlink(Result<(), WireError>),
    HardLink(Result<(), WireError>),
    SymlinkMetadata(Result<Metadata, WireError>),
    SetMetadata(Result<(), WireError>),
    Canonicalize(Result<WirePath, WireError>),
    ReadLink(Result<WirePath, WireError>),
    Access(Result<(), WireError>),
    Glob(Result<Vec<WirePath>, WireError>),
    Xattrs(Result<Vec<XattrEntry>, WireError>),
    Xattr(Result<Vec<u8>, WireError>),
    SetXattr(Result<(), WireError>),
    RemoveXattr(Result<(), WireError>),
    Streams(Result<Vec<StreamEntry>, WireError>),
    Extension(Result<ExtensionResponse, WireError>),
}
