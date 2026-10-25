use std::{any::Any, collections::HashMap, fmt};

use dolang_rpc::{
    Protocol,
    auth::AuthKey,
    handle::OsHandle,
    session::{Cite, Gift},
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, SeqAccess, Visitor},
    ser::SerializeTuple,
};

use crate::{
    directory::DirEntry,
    error::Error,
    extension::ExtensionSet,
    extension::{self, ErasedVfsExtension},
    file::{CopyDataResult, CopyDest, CopyMode, FileLockRequest, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{FsMetadata, Metadata, MetadataPatch},
    path::{self, WellKnownPath},
    process::{
        ProcessControl, ProcessExit, ProcessInfo, ProcessStatus, Signal, StartTime,
        TerminationPolicy,
    },
    security::{Acl, AclKind, PrincipalId, PrincipalIdKind, SecurityInfo, SidName},
    session::{
        ChildMarker, FileLockMarker, FileMarker, ProcessEnumMarker, ProcessMarker, ReadDirMarker,
        StdioRecvMarker, StdioSendMarker, VfsMarker,
    },
    target::TargetInfo,
};
use dolang_winterop::security::{SecDesc, SecInfo, Sid};
use uuid::Uuid;

pub(crate) struct VfsProtocol;

impl Protocol for VfsProtocol {
    type Request = Request;
    type Response = Result<ResponseKind, Error>;
}

/// Application-protocol name/version advertised during the RPC handshake.
/// `dolang_rpc::server::Server` and `dolang_rpc::client::Client` are only
/// reachable via their respective `Unbound` endpoints, which require this
/// descriptor.
pub(crate) const APP_PROTOCOL: (&str, &[u16]) = ("dolang-vfs", &[1]);

/// Starts a fresh [`dolang_rpc::Builder`] preconfigured with
/// [`APP_PROTOCOL`], optionally requiring mutual proof of a pre-shared key.
///
/// This is the single point at which every VFS transport acquires its RPC
/// builder, so a key supplied here reaches whichever `client*`/`server*`
/// method the caller goes on to use.
pub(crate) fn rpc_builder(key: Option<AuthKey>) -> dolang_rpc::Builder {
    let builder = dolang_rpc::Builder::new(APP_PROTOCOL.0, APP_PROTOCOL.1);
    match key {
        Some(key) => builder.key(key),
        None => builder,
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Request {
    pub(crate) vfs: Option<Cite<VfsMarker>>,
    pub(crate) kind: RequestKind,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct QueryResponse {
    pub(crate) session: Uuid,
    pub(crate) pid: u32,
    pub(crate) env: HashMap<String, String>,
    pub(crate) cwd: path::PathBuf,
    pub(crate) current_exe: path::PathBuf,
    pub(crate) target: TargetInfo,
    pub(crate) security: SecurityInfo,
    pub(crate) extensions: ExtensionSet,
}

#[cfg(test)]
mod tests {
    use crate::{
        error::{Error, ErrorKind},
        target::OperatingSystem,
    };

    #[test]
    fn wire_error_preserves_foreign_system_error() {
        let error = Error::from_system_code(
            ErrorKind::PermissionDenied,
            "access is denied",
            OperatingSystem::Windows,
            5,
        );

        let system = error.system_code().unwrap();
        assert_eq!(system.operating_system(), OperatingSystem::Windows);
        assert_eq!(system.raw(), 5);
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(error.message(), "access is denied");
    }

    #[test]
    fn wire_error_preserves_incidental_io_error() {
        let error = Error::new(ErrorKind::InvalidData, "bad reply");

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
    pub(crate) program: path::PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, Option<String>>,
    pub(crate) cwd: Option<path::PathBuf>,
    pub(crate) stdin: StdioRecvTarget,
    pub(crate) stdout: StdioSendTarget,
    pub(crate) stderr: StdioSendTarget,
    pub(crate) process_control: ProcessControl,
    pub(crate) termination_policy: TerminationPolicy,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct PipeResponse {
    pub(crate) send: Gift<StdioSendMarker>,
    pub(crate) recv: Gift<StdioRecvMarker>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum StdioRecvTarget {
    Null,
    Native(OsHandle),
    Opaque(Cite<StdioRecvMarker>),
    /// A file retained here, read from `pos` on.
    File {
        file: Cite<FileMarker>,
        pos: u64,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum StdioSendTarget {
    Null,
    Stdout,
    Native(OsHandle),
    Opaque(Cite<StdioSendMarker>),
    /// A file retained here, written from `pos` on.
    File {
        file: Cite<FileMarker>,
        pos: u64,
    },
}

/// How a spawned child ended, and where it left the files it was given.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ChildExit {
    pub(crate) status: ProcessStatus,
    /// The final position of each [`StdioRecvTarget::File`] and
    /// [`StdioSendTarget::File`] target, in stdin, stdout, stderr order.
    pub(crate) positions: Vec<u64>,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub(crate) struct OpenFlags: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const APPEND = 1 << 2;
        const CREATE = 1 << 3;
        const CREATE_NEW = 1 << 4;
        const TRUNCATE = 1 << 5;
        const NO_FOLLOW = 1 << 6;
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct OpenRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) flags: OpenFlags,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ReadDirPage {
    pub(crate) entries: Vec<DirEntry>,
    pub(crate) done: bool,
}

/// One page of a process-table enumeration.
///
/// Unlike [`ReadDirPage`], the page boundary is chosen by payload size rather
/// than entry count: a record carrying a command line is unbounded in a way a
/// directory entry is not, and a few thousand of them is megabytes.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ProcessPage {
    pub(crate) entries: Vec<ProcessInfo>,
    pub(crate) done: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum OpenHandle {
    Native(OsHandle),
    Opaque(Gift<FileMarker>),
}

/// `Debug` is manual so a request cannot print the key it carries.
#[derive(Serialize, Deserialize)]
pub(crate) struct UnixVfsRequest {
    pub(crate) path: path::PathBuf,
    /// Pre-shared key for the nested connection, when the peer must establish
    /// it on our behalf. Only the non-handle-passing path uses this: when the
    /// peer can return a connected descriptor, negotiation — and therefore
    /// authentication — happens locally and the key never leaves this process.
    pub(crate) key: Option<Vec<u8>>,
}

impl fmt::Debug for UnixVfsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixVfsRequest")
            .field("path", &self.path)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WindowsAdminRequest {
    pub(crate) cwd: path::PathBuf,
    pub(crate) env: HashMap<String, Option<String>>,
    pub(crate) elevate: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum OpenVfsHandle {
    Native(OsHandle),
    Opaque(Gift<VfsMarker>),
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CreateDirRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveDirRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MetadataRequest {
    pub(crate) path: path::PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct FsMetadataRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AclRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) kind: AclKind,
    pub(crate) default: bool,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetAclRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) kind: AclKind,
    pub(crate) acl: Option<Acl>,
    pub(crate) default: bool,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SecDescRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) mask: SecInfo,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct UpdateSecDescRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) sec_desc: SecDesc,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct UpdateMetadataRequest {
    pub(crate) paths: Vec<path::PathBuf>,
    pub(crate) patch: MetadataPatch,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CopyRequest {
    pub(crate) from: path::PathBuf,
    pub(crate) to: path::PathBuf,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RenameRequest {
    pub(crate) from: path::PathBuf,
    pub(crate) to: path::PathBuf,
    pub(crate) replace: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MoveRequest {
    pub(crate) from: path::PathBuf,
    pub(crate) to: path::PathBuf,
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
    pub(crate) cwd: path::PathBuf,
    pub(crate) src: path::PathBuf,
    pub(crate) dst: path::PathBuf,
    pub(crate) kind: SymlinkKind,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct HardLinkRequest {
    pub(crate) src: path::PathBuf,
    pub(crate) dst: path::PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CanonicalizeRequest {
    pub(crate) path: path::PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ReadLinkRequest {
    pub(crate) path: path::PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AccessRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) mode: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct GlobRequest {
    pub(crate) pattern: String,
    pub(crate) root: path::PathBuf,
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
    pub(crate) path: path::PathBuf,
    pub(crate) namespace: XattrNamespaceRequest,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct StreamsRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct XattrRequest {
    pub(crate) path: path::PathBuf,
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetXattrRequest {
    pub(crate) path: path::PathBuf,
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
    pub(crate) payload: Box<dyn Any + Send>,
}

impl fmt::Debug for ExtensionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionRequest")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Serialize for ExtensionRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ext = extension::lookup(&self.name, self.version)
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
    type Value = Box<dyn Any + Send>;
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

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a VFS extension request")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let name: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let version: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                let ext = extension::lookup(&name, version)
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
    pub(crate) payload: Box<dyn Any + Send>,
}

impl fmt::Debug for ExtensionResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionResponse")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Serialize for ExtensionResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ext = extension::lookup(&self.name, self.version)
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
    type Value = Box<dyn Any + Send>;
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

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a VFS extension response")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let name: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let version: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                let ext = extension::lookup(&name, version)
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
        child: Cite<ChildMarker>,
    },
    ChildTerminate {
        child: Cite<ChildMarker>,
    },
    ChildClose {
        child: Cite<ChildMarker>,
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
    ResolvePrincipalId {
        input: PrincipalId,
        want: PrincipalIdKind,
    },
    Which {
        program: path::PathBuf,
        path: Option<String>,
        cwd: Option<path::PathBuf>,
    },
    WellKnownPath(WellKnownPathRequest),
    Stop,
    ClearCache,
    Pipe {
        buf_size: Option<usize>,
    },
    Open(OpenRequest),
    FileRead {
        file: Cite<FileMarker>,
        offset: u64,
        len: usize,
    },
    FileWrite {
        file: Cite<FileMarker>,
        offset: u64,
    },
    FileAppend {
        file: Cite<FileMarker>,
    },
    FileSize {
        file: Cite<FileMarker>,
    },
    FileSetSize {
        file: Cite<FileMarker>,
        size: u64,
    },
    FileSync {
        file: Cite<FileMarker>,
        data: bool,
    },
    FileLock {
        file: Cite<FileMarker>,
        request: FileLockRequest,
    },
    FileUnlock {
        lock: Cite<FileLockMarker>,
    },
    StdioSendClose {
        stdio: Cite<StdioSendMarker>,
    },
    StdioSendWrite {
        stdio: Cite<StdioSendMarker>,
    },
    StdioSendClone {
        stdio: Cite<StdioSendMarker>,
    },
    StdioRecvClose {
        stdio: Cite<StdioRecvMarker>,
    },
    StdioRecvRead {
        stdio: Cite<StdioRecvMarker>,
        len: usize,
    },
    StdioRecvClone {
        stdio: Cite<StdioRecvMarker>,
    },
    FileMetadata {
        file: Cite<FileMarker>,
    },
    FileFsMetadata {
        file: Cite<FileMarker>,
    },
    FileAcl {
        file: Cite<FileMarker>,
        kind: AclKind,
        default: bool,
    },
    FileSetAcl {
        file: Cite<FileMarker>,
        kind: AclKind,
        acl: Option<Acl>,
        default: bool,
    },
    FileSecDesc {
        file: Cite<FileMarker>,
        mask: SecInfo,
    },
    FileUpdateSecDesc {
        file: Cite<FileMarker>,
        sec_desc: SecDesc,
    },
    FileXattrs {
        file: Cite<FileMarker>,
        namespace: XattrNamespaceRequest,
    },
    FileXattr {
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
    },
    FileStreams {
        file: Cite<FileMarker>,
    },
    FileSetXattr {
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
        value: Vec<u8>,
    },
    FileRemoveXattr {
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
    },
    FileClose {
        file: Cite<FileMarker>,
    },
    UnixVfs(UnixVfsRequest),
    WindowsAdmin(WindowsAdminRequest),
    ReadDir {
        path: path::PathBuf,
    },
    ReadDirNext {
        read_dir: Cite<ReadDirMarker>,
    },
    ReadDirClose {
        read_dir: Cite<ReadDirMarker>,
    },
    Remove(RemoveRequest),
    Metadata(MetadataRequest),
    FsMetadata(FsMetadataRequest),
    Acl(AclRequest),
    SetAcl(SetAclRequest),
    SecDesc(SecDescRequest),
    UpdateSecDesc(UpdateSecDescRequest),
    CreateDir(CreateDirRequest),
    RemoveDir(RemoveDirRequest),
    Copy(CopyRequest),
    Rename(RenameRequest),
    Move(MoveRequest),
    Symlink(SymlinkRequest),
    HardLink(HardLinkRequest),
    SymlinkMetadata(MetadataRequest),
    UpdateMetadata(UpdateMetadataRequest),
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
    // Appended, and every future variant must be too: these enums are
    // externally tagged with no explicit discriminants, so the wire form of a
    // variant is its position. Inserting anywhere else silently renumbers
    // everything after it.
    //
    // There is no version gate on this request, which assumes a peer built
    // from the same tree: an older server fails to decode it and drops the
    // session rather than answering `Unsupported`. If mixed versions ever
    // become possible, advertise `APP_PROTOCOL` version 2, capture
    // `UnboundClient::version()` before `bind()` at each client construction
    // site, and have `client::File::can_copy_data_with` fall through to the
    // relay against a version-1 peer.
    FileCopyData {
        src: Cite<FileMarker>,
        dst: Cite<FileMarker>,
        src_offset: u64,
        target: CopyDest,
        len: Option<u64>,
        mode: CopyMode,
    },
    /// Opens a forward enumeration of the target's process table. Appended for
    /// the reason given above.
    ProcessEnumerate,
    ProcessEnumerateNext {
        processes: Cite<ProcessEnumMarker>,
    },
    ProcessEnumerateClose {
        processes: Cite<ProcessEnumMarker>,
    },
    ProcessOpen {
        pid: u32,
        /// The start time the caller expects, when it has one to check. `None`
        /// opens whatever currently owns `pid`.
        start: Option<StartTime>,
    },
    ProcessInfo {
        process: Cite<ProcessMarker>,
    },
    ProcessSignal {
        process: Cite<ProcessMarker>,
        signal: Signal,
    },
    ProcessTerminate {
        process: Cite<ProcessMarker>,
    },
    ProcessWait {
        process: Cite<ProcessMarker>,
    },
    ProcessClose {
        process: Cite<ProcessMarker>,
    },
    /// Kills the process outright. Appended for the reason given above.
    ProcessKill {
        process: Cite<ProcessMarker>,
    },
    /// Describes one process by PID, without opening a handle to it. Appended
    /// for the reason given above.
    ProcessDescribe {
        pid: u32,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum ResponseKind {
    Spawn(Gift<ChildMarker>),
    ChildWait(ChildExit),
    ChildTerminate(Option<ChildExit>),
    ChildClose,
    Query(QueryResponse),
    UserName(String),
    UserId(u32),
    GroupName(String),
    GroupId(u32),
    SidName(SidName),
    AccountName(SidName),
    ResolvePrincipalId(PrincipalId),
    Which(Option<path::PathBuf>),
    WellKnownPath(Option<path::PathBuf>),
    Stop,
    ClearCache,
    Pipe(PipeResponse),
    Open(OpenHandle),
    FileRead,
    FileWrite(usize),
    FileAppend((usize, u64)),
    FileSize(u64),
    FileSetSize,
    FileSync,
    FileLock(Option<Gift<FileLockMarker>>),
    FileUnlock,
    StdioSendClose,
    StdioSendWrite(usize),
    StdioSendClone(Gift<StdioSendMarker>),
    StdioRecvClose,
    StdioRecvRead,
    StdioRecvClone(Gift<StdioRecvMarker>),
    FileMetadata(Metadata),
    FileFsMetadata(FsMetadata),
    FileAcl(Option<Acl>),
    FileSetAcl,
    FileSecDesc(SecDesc),
    FileUpdateSecDesc,
    FileXattrs(Vec<XattrEntry>),
    FileXattr(Vec<u8>),
    FileStreams(Vec<StreamEntry>),
    FileSetXattr,
    FileRemoveXattr,
    FileClose,
    UnixVfs(OpenVfsHandle),
    WindowsAdmin(Gift<VfsMarker>),
    ReadDir(Gift<ReadDirMarker>),
    ReadDirNext(ReadDirPage),
    ReadDirClose,
    Remove,
    Metadata(Metadata),
    FsMetadata(FsMetadata),
    Acl(Option<Acl>),
    SetAcl,
    SecDesc(SecDesc),
    UpdateSecDesc,
    CreateDir,
    RemoveDir,
    Copy,
    Rename,
    Move,
    Symlink,
    HardLink,
    SymlinkMetadata(Metadata),
    UpdateMetadata,
    Canonicalize(path::PathBuf),
    ReadLink(path::PathBuf),
    Access,
    Glob(Vec<path::PathBuf>),
    Xattrs(Vec<XattrEntry>),
    Xattr(Vec<u8>),
    SetXattr,
    RemoveXattr,
    Streams(Vec<StreamEntry>),
    Extension(ExtensionResponse),
    /// Bounded copy result. Appended for the reason given on `RequestKind`.
    FileCopyData(CopyDataResult),
    ProcessEnumerate(Gift<ProcessEnumMarker>),
    ProcessEnumerateNext(ProcessPage),
    ProcessEnumerateClose,
    ProcessOpen(Gift<ProcessMarker>),
    ProcessInfo(ProcessInfo),
    ProcessSignal,
    ProcessTerminate,
    ProcessWait(ProcessExit),
    ProcessClose,
    ProcessKill,
    ProcessDescribe(ProcessInfo),
}
