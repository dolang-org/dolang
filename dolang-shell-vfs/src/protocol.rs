use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio_unix_ipc::serde::Handle;

pub(crate) use crate::{Attrs, ChownIdentity, Metadata, WellKnownPath};

pub(crate) type RequestId = u64;

#[derive(Serialize, Deserialize)]
pub(crate) struct SpawnRequest {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, Option<String>>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) stdin_fd: Option<Handle<OwnedFd>>,
    pub(crate) stdout_fd: Option<Handle<OwnedFd>>,
    pub(crate) stderr_fd: Option<Handle<OwnedFd>>,
}

impl std::fmt::Debug for SpawnRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnRequest")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .field("stdin_fd", &self.stdin_fd.is_some())
            .field("stdout_fd", &self.stdout_fd.is_some())
            .field("stderr_fd", &self.stderr_fd.is_some())
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct OpenRequest {
    pub(crate) path: PathBuf,
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) append: bool,
    pub(crate) create: bool,
    pub(crate) create_new: bool,
    pub(crate) truncate: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct UnixStreamSocketRequest {
    pub(crate) bind: Option<PathBuf>,
    pub(crate) connect: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveRequest {
    pub(crate) path: PathBuf,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CreateDirRequest {
    pub(crate) path: PathBuf,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RemoveDirRequest {
    pub(crate) path: PathBuf,
    pub(crate) all: bool,
    pub(crate) ignore: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MetadataRequest {
    pub(crate) path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AttrsRequest {
    pub(crate) path: PathBuf,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetAttrsRequest {
    pub(crate) path: PathBuf,
    pub(crate) attrs: Attrs,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CopyRequest {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RenameRequest {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MoveRequest {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
    pub(crate) all: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SymlinkRequest {
    pub(crate) src: PathBuf,
    pub(crate) dst: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct HardLinkRequest {
    pub(crate) src: PathBuf,
    pub(crate) dst: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CanonicalizeRequest {
    pub(crate) path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ReadLinkRequest {
    pub(crate) path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AccessRequest {
    pub(crate) path: PathBuf,
    pub(crate) mode: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct GlobRequest {
    pub(crate) pattern: String,
    pub(crate) root: PathBuf,
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
    pub(crate) path: PathBuf,
    pub(crate) mode: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) struct Timestamp {
    pub(crate) secs: i64,
    pub(crate) nanos: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SetTimesRequest {
    pub(crate) path: PathBuf,
    pub(crate) accessed: Option<Timestamp>,
    pub(crate) modified: Option<Timestamp>,
    pub(crate) created: Option<Timestamp>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ChownRequest {
    pub(crate) path: PathBuf,
    pub(crate) user: Option<ChownIdentity>,
    pub(crate) group: Option<ChownIdentity>,
    pub(crate) follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum RequestKind {
    Spawn(SpawnRequest),
    Cancel,
    Query,
    Which {
        program: PathBuf,
        path: Option<String>,
        cwd: Option<PathBuf>,
    },
    WellKnownPath(WellKnownPathRequest),
    Stop,
    ClearCache,
    Open(OpenRequest),
    UnixStreamSocket(UnixStreamSocketRequest),
    Remove(RemoveRequest),
    Metadata(MetadataRequest),
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
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Request {
    pub(crate) id: RequestId,
    pub(crate) kind: RequestKind,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum ResponseKind {
    Spawn(Result<i32, i32>),
    Cancel,
    Query {
        env: HashMap<String, String>,
        cwd: PathBuf,
    },
    Which(Option<PathBuf>),
    WellKnownPath(Result<PathBuf, i32>),
    Stop,
    ClearCache,
    Open(Result<Handle<OwnedFd>, i32>),
    UnixStreamSocket(Result<Handle<OwnedFd>, i32>),
    Remove(Result<(), i32>),
    Metadata(Result<Metadata, i32>),
    CreateDir(Result<(), i32>),
    RemoveDir(Result<(), i32>),
    Copy(Result<(), i32>),
    Rename(Result<(), i32>),
    Move(Result<(), i32>),
    Symlink(Result<(), i32>),
    HardLink(Result<(), i32>),
    SymlinkMetadata(Result<Metadata, i32>),
    Attrs(Result<Attrs, i32>),
    SetAttrs(Result<(), i32>),
    Canonicalize(Result<PathBuf, i32>),
    ReadLink(Result<PathBuf, i32>),
    Access(Result<(), i32>),
    Glob(Result<Vec<PathBuf>, i32>),
    SetPermissions(Result<(), i32>),
    SetTimes(Result<(), i32>),
    Chown(Result<(), i32>),
}

impl std::fmt::Debug for ResponseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseKind::Spawn(result) => f.debug_tuple("Spawn").field(result).finish(),
            ResponseKind::Cancel => f.debug_struct("Cancel").finish(),
            ResponseKind::Query { env, cwd } => f
                .debug_struct("Query")
                .field("env", env)
                .field("cwd", cwd)
                .finish(),
            ResponseKind::Which(path) => f.debug_tuple("Which").field(path).finish(),
            ResponseKind::WellKnownPath(result) => {
                f.debug_tuple("WellKnownPath").field(result).finish()
            }
            ResponseKind::Stop => f.debug_struct("Stop").finish(),
            ResponseKind::ClearCache => f.debug_struct("ClearCache").finish(),
            ResponseKind::Open(result) => f
                .debug_tuple("Open")
                .field(&result.as_ref().map(|_| "<fd>"))
                .finish(),
            ResponseKind::UnixStreamSocket(result) => f
                .debug_tuple("UnixStreamSocket")
                .field(&result.as_ref().map(|_| "<fd>"))
                .finish(),
            ResponseKind::Remove(result) => f.debug_tuple("Remove").field(result).finish(),
            ResponseKind::Metadata(result) => f.debug_tuple("Metadata").field(result).finish(),
            ResponseKind::CreateDir(result) => f.debug_tuple("CreateDir").field(result).finish(),
            ResponseKind::RemoveDir(result) => f.debug_tuple("RemoveDir").field(result).finish(),
            ResponseKind::Copy(result) => f.debug_tuple("Copy").field(result).finish(),
            ResponseKind::Rename(result) => f.debug_tuple("Rename").field(result).finish(),
            ResponseKind::Move(result) => f.debug_tuple("Move").field(result).finish(),
            ResponseKind::Symlink(result) => f.debug_tuple("Symlink").field(result).finish(),
            ResponseKind::HardLink(result) => f.debug_tuple("HardLink").field(result).finish(),
            ResponseKind::SymlinkMetadata(result) => {
                f.debug_tuple("SymlinkMetadata").field(result).finish()
            }
            ResponseKind::Attrs(result) => f.debug_tuple("Attrs").field(result).finish(),
            ResponseKind::SetAttrs(result) => f.debug_tuple("SetAttrs").field(result).finish(),
            ResponseKind::Canonicalize(result) => {
                f.debug_tuple("Canonicalize").field(result).finish()
            }
            ResponseKind::ReadLink(result) => f.debug_tuple("ReadLink").field(result).finish(),
            ResponseKind::Access(result) => f.debug_tuple("Access").field(result).finish(),
            ResponseKind::Glob(result) => f
                .debug_tuple("Glob")
                .field(&result.as_ref().map(|v| format!("{} paths", v.len())))
                .finish(),
            ResponseKind::SetPermissions(result) => {
                f.debug_tuple("SetPermissions").field(result).finish()
            }
            ResponseKind::SetTimes(result) => f.debug_tuple("SetTimes").field(result).finish(),
            ResponseKind::Chown(result) => f.debug_tuple("Chown").field(result).finish(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Response {
    pub(crate) id: RequestId,
    pub(crate) kind: ResponseKind,
}
