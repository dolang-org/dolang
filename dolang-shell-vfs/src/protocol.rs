use std::collections::HashMap;
use std::{io, path::PathBuf};

use dolang_rpc::{OsHandle, Protocol};
use serde::{Deserialize, Serialize};

pub(crate) use crate::{
    Attrs, ChownIdentity, DirEntry, FsMetadata, Metadata, OperatingSystem, StreamEntry, TargetInfo,
    WellKnownPath, XattrEntry, XattrNamespace,
};

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) enum WireErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    NetworkDown,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StaleNetworkFileHandle,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    StorageFull,
    NotSeekable,
    QuotaExceeded,
    FileTooLarge,
    ResourceBusy,
    ExecutableFileBusy,
    Deadlock,
    CrossesDevices,
    TooManyLinks,
    InvalidFilename,
    ArgumentListTooLong,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
}

impl From<io::ErrorKind> for WireErrorKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::AddrInUse => Self::AddrInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
            io::ErrorKind::NetworkDown => Self::NetworkDown,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::NotADirectory => Self::NotADirectory,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            io::ErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::StorageFull => Self::StorageFull,
            io::ErrorKind::NotSeekable => Self::NotSeekable,
            io::ErrorKind::QuotaExceeded => Self::QuotaExceeded,
            io::ErrorKind::FileTooLarge => Self::FileTooLarge,
            io::ErrorKind::ResourceBusy => Self::ResourceBusy,
            io::ErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            io::ErrorKind::Deadlock => Self::Deadlock,
            io::ErrorKind::CrossesDevices => Self::CrossesDevices,
            io::ErrorKind::TooManyLinks => Self::TooManyLinks,
            io::ErrorKind::InvalidFilename => Self::InvalidFilename,
            io::ErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            _ => Self::Other,
        }
    }
}

impl From<WireErrorKind> for io::ErrorKind {
    fn from(kind: WireErrorKind) -> Self {
        match kind {
            WireErrorKind::NotFound => Self::NotFound,
            WireErrorKind::PermissionDenied => Self::PermissionDenied,
            WireErrorKind::ConnectionRefused => Self::ConnectionRefused,
            WireErrorKind::ConnectionReset => Self::ConnectionReset,
            WireErrorKind::HostUnreachable => Self::HostUnreachable,
            WireErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            WireErrorKind::ConnectionAborted => Self::ConnectionAborted,
            WireErrorKind::NotConnected => Self::NotConnected,
            WireErrorKind::AddrInUse => Self::AddrInUse,
            WireErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
            WireErrorKind::NetworkDown => Self::NetworkDown,
            WireErrorKind::BrokenPipe => Self::BrokenPipe,
            WireErrorKind::AlreadyExists => Self::AlreadyExists,
            WireErrorKind::WouldBlock => Self::WouldBlock,
            WireErrorKind::NotADirectory => Self::NotADirectory,
            WireErrorKind::IsADirectory => Self::IsADirectory,
            WireErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            WireErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            WireErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            WireErrorKind::InvalidInput => Self::InvalidInput,
            WireErrorKind::InvalidData => Self::InvalidData,
            WireErrorKind::TimedOut => Self::TimedOut,
            WireErrorKind::WriteZero => Self::WriteZero,
            WireErrorKind::StorageFull => Self::StorageFull,
            WireErrorKind::NotSeekable => Self::NotSeekable,
            WireErrorKind::QuotaExceeded => Self::QuotaExceeded,
            WireErrorKind::FileTooLarge => Self::FileTooLarge,
            WireErrorKind::ResourceBusy => Self::ResourceBusy,
            WireErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            WireErrorKind::Deadlock => Self::Deadlock,
            WireErrorKind::CrossesDevices => Self::CrossesDevices,
            WireErrorKind::TooManyLinks => Self::TooManyLinks,
            WireErrorKind::InvalidFilename => Self::InvalidFilename,
            WireErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            WireErrorKind::Interrupted => Self::Interrupted,
            WireErrorKind::Unsupported => Self::Unsupported,
            WireErrorKind::UnexpectedEof => Self::UnexpectedEof,
            WireErrorKind::OutOfMemory => Self::OutOfMemory,
            WireErrorKind::Other => Self::Other,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum WireError {
    Io {
        kind: WireErrorKind,
        message: String,
    },
    System {
        operating_system: OperatingSystem,
        code: i32,
        kind: WireErrorKind,
        message: String,
    },
}

impl From<crate::Error> for WireError {
    fn from(error: crate::Error) -> Self {
        match error {
            crate::Error::Io(error) => Self::Io {
                kind: error.kind().into(),
                message: error.to_string(),
            },
            crate::Error::System(error) => Self::System {
                operating_system: error.operating_system().clone(),
                code: error.code(),
                kind: error.kind().into(),
                message: error.message().to_owned(),
            },
        }
    }
}

impl From<WireError> for crate::Error {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Io { kind, message } => {
                Self::Io(io::Error::new(io::ErrorKind::from(kind), message))
            }
            WireError::System {
                operating_system,
                code,
                kind,
                message,
            } => Self::System(crate::SystemError::new(
                operating_system,
                code,
                kind.into(),
                message,
            )),
        }
    }
}

pub(crate) struct VfsProtocol;

impl Protocol for VfsProtocol {
    type Request = RequestKind;
    type Response = ResponseKind;
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
    use std::io;

    use super::{ResponseKind, WireError, WirePath, WirePathKind};
    use crate::{
        Architecture, Error, OperatingSystem, SystemError, TargetInfo, Utf8TypedPath,
        Utf8TypedPathBuf, Utf8UnixPath, Utf8WindowsPath,
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
        let error = Error::System(SystemError::new(
            OperatingSystem::Windows,
            5,
            io::ErrorKind::PermissionDenied,
            "access is denied",
        ));

        let error = Error::from(WireError::from(error));
        let system = error.system().unwrap();
        assert_eq!(system.operating_system(), &OperatingSystem::Windows);
        assert_eq!(system.code(), 5);
        assert_eq!(system.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(system.message(), "access is denied");
    }

    #[test]
    fn wire_error_preserves_incidental_io_error() {
        let error = Error::Io(io::Error::new(io::ErrorKind::InvalidData, "bad reply"));

        let error = Error::from(WireError::from(error));
        assert!(error.system().is_none());
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "bad reply");
    }

    #[test]
    fn wire_query_preserves_cross_target_identity() {
        let response = ResponseKind::Query {
            env: [("Path".to_owned(), r"C:\Windows".to_owned())].into(),
            cwd: WirePath::from(Utf8TypedPath::Windows(Utf8WindowsPath::new(r"C:\work"))),
            target: TargetInfo {
                operating_system: OperatingSystem::Windows,
                architecture: Architecture::Aarch64,
                logical_cpu_count: 24,
                is_wine: Some(true),
            },
        };

        let encoded = postcard::to_stdvec(&response).unwrap();
        let decoded: ResponseKind = postcard::from_bytes(&encoded).unwrap();
        let ResponseKind::Query { env, cwd, target } = decoded else {
            panic!("query response changed variant");
        };

        assert_eq!(env["Path"], r"C:\Windows");
        assert_eq!(Utf8TypedPathBuf::from(cwd).as_str(), r"C:\work");
        assert_eq!(target.operating_system, OperatingSystem::Windows);
        assert_eq!(target.architecture, Architecture::Aarch64);
        assert_eq!(target.logical_cpu_count, 24);
        assert_eq!(target.is_wine, Some(true));
    }

    use std::path::PathBuf;
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
    pub(crate) stdin_fd: Option<OsHandle>,
    pub(crate) stdout_fd: Option<OsHandle>,
    pub(crate) stderr_fd: Option<OsHandle>,
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
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct UnixStreamSocketRequest {
    pub(crate) bind: Option<WirePath>,
    pub(crate) connect: Option<WirePath>,
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
pub(crate) struct AttrsRequest {
    pub(crate) path: WirePath,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetAttrsRequest {
    pub(crate) path: WirePath,
    pub(crate) attrs: Attrs,
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
    pub(crate) env: HashMap<String, Option<String>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetPermissionsRequest {
    pub(crate) path: WirePath,
    pub(crate) mode: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) struct Timestamp {
    pub(crate) secs: i64,
    pub(crate) nanos: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetTimesRequest {
    pub(crate) path: WirePath,
    pub(crate) accessed: Option<Timestamp>,
    pub(crate) modified: Option<Timestamp>,
    pub(crate) created: Option<Timestamp>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ChownRequest {
    pub(crate) path: WirePath,
    pub(crate) user: Option<ChownIdentity>,
    pub(crate) group: Option<ChownIdentity>,
    pub(crate) follow: bool,
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

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum RequestKind {
    Spawn(SpawnRequest),
    Query,
    Which {
        program: WirePath,
        path: Option<String>,
        cwd: Option<WirePath>,
    },
    WellKnownPath(WellKnownPathRequest),
    Stop,
    ClearCache,
    Open(OpenRequest),
    UnixStreamSocket(UnixStreamSocketRequest),
    ReadDir {
        path: WirePath,
    },
    Remove(RemoveRequest),
    Metadata(MetadataRequest),
    FsMetadata(FsMetadataRequest),
    CreateDir(CreateDirRequest),
    RemoveDir(RemoveDirRequest),
    Copy(CopyRequest),
    Rename(RenameRequest),
    Move(MoveRequest),
    Symlink(SymlinkRequest),
    HardLink(HardLinkRequest),
    SymlinkMetadata(MetadataRequest),
    Attrs(AttrsRequest),
    SetAttrs(SetAttrsRequest),
    Canonicalize(CanonicalizeRequest),
    ReadLink(ReadLinkRequest),
    Access(AccessRequest),
    Glob(GlobRequest),
    SetPermissions(SetPermissionsRequest),
    SetTimes(SetTimesRequest),
    Chown(ChownRequest),
    Xattrs(XattrsRequest),
    Xattr(XattrRequest),
    SetXattr(SetXattrRequest),
    RemoveXattr(XattrRequest),
    Streams(StreamsRequest),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum ResponseKind {
    Spawn(Result<i32, WireError>),
    Query {
        env: HashMap<String, String>,
        cwd: WirePath,
        target: TargetInfo,
    },
    Which(Option<WirePath>),
    WellKnownPath(Result<WirePath, WireError>),
    Stop,
    ClearCache,
    Open(Result<OsHandle, WireError>),
    UnixStreamSocket(Result<OsHandle, WireError>),
    ReadDir(Result<Vec<DirEntry>, WireError>),
    Remove(Result<(), WireError>),
    Metadata(Result<Metadata, WireError>),
    FsMetadata(Result<FsMetadata, WireError>),
    CreateDir(Result<(), WireError>),
    RemoveDir(Result<(), WireError>),
    Copy(Result<(), WireError>),
    Rename(Result<(), WireError>),
    Move(Result<(), WireError>),
    Symlink(Result<(), WireError>),
    HardLink(Result<(), WireError>),
    SymlinkMetadata(Result<Metadata, WireError>),
    Attrs(Result<Attrs, WireError>),
    SetAttrs(Result<(), WireError>),
    Canonicalize(Result<WirePath, WireError>),
    ReadLink(Result<WirePath, WireError>),
    Access(Result<(), WireError>),
    Glob(Result<Vec<WirePath>, WireError>),
    SetPermissions(Result<(), WireError>),
    SetTimes(Result<(), WireError>),
    Chown(Result<(), WireError>),
    Xattrs(Result<Vec<XattrEntry>, WireError>),
    Xattr(Result<Vec<u8>, WireError>),
    SetXattr(Result<(), WireError>),
    RemoveXattr(Result<(), WireError>),
    Streams(Result<Vec<StreamEntry>, WireError>),
}
