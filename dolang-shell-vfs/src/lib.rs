#![deny(warnings)]
#![allow(async_fn_in_trait)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::fd::OwnedFd;
use std::{
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
};
use tokio::fs;

#[cfg(unix)]
mod client;
mod direct;
mod pipe;
#[cfg(unix)]
mod protocol;
mod read_dir;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod service;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Fifo,
    CharacterDevice,
    BlockDevice,
    Socket,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    mode: u32,
}

impl Permissions {
    pub fn from_mode(mode: u32) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }

    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o200;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub len: u64,
    pub file_type: FileType,
    pub atime: i64,
    pub atime_nsec: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blksize: u64,
    pub blocks: u64,
    #[cfg(windows)]
    pub win_attrs: u32,
    #[cfg(target_os = "macos")]
    pub unix_flags: u32,
}

impl Metadata {
    pub fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.mode)
    }

    pub fn attrs(&self) -> Attrs {
        #[cfg(any(windows, target_os = "macos"))]
        {
            #[cfg(windows)]
            {
                Attrs::from_win_attrs(self.win_attrs)
            }
            #[cfg(target_os = "macos")]
            {
                Attrs::from_macos_flags(self.unix_flags)
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Attrs::default()
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub readonly: Option<bool>,
    pub hidden: Option<bool>,
    pub system: Option<bool>,
    pub archive: Option<bool>,
    pub reparse_point: Option<bool>,
    pub compressed: Option<bool>,
    pub encrypted: Option<bool>,
    pub temporary: Option<bool>,
    pub offline: Option<bool>,
    pub not_content_indexed: Option<bool>,
    pub immutable: Option<bool>,
    pub append_only: Option<bool>,
    pub no_dump: Option<bool>,
    pub no_atime: Option<bool>,
    pub no_copy_on_write: Option<bool>,
    pub dir_sync: Option<bool>,
    pub casefold: Option<bool>,
    pub data_journaling: Option<bool>,
    pub no_compress: Option<bool>,
    pub project_inherit: Option<bool>,
    pub secure_delete: Option<bool>,
    pub sync: Option<bool>,
    pub no_tail_merge: Option<bool>,
    pub top_dir: Option<bool>,
    pub undelete: Option<bool>,
    pub direct_access: Option<bool>,
    pub extent_format: Option<bool>,
    pub opaque: Option<bool>,
    pub win_attrs: Option<u32>,
    pub unix_flags: Option<u32>,
}

impl Attrs {
    pub fn is_empty_patch(&self) -> bool {
        self.readonly.is_none()
            && self.hidden.is_none()
            && self.system.is_none()
            && self.archive.is_none()
            && self.reparse_point.is_none()
            && self.compressed.is_none()
            && self.encrypted.is_none()
            && self.temporary.is_none()
            && self.offline.is_none()
            && self.not_content_indexed.is_none()
            && self.immutable.is_none()
            && self.append_only.is_none()
            && self.no_dump.is_none()
            && self.no_atime.is_none()
            && self.no_copy_on_write.is_none()
            && self.dir_sync.is_none()
            && self.casefold.is_none()
            && self.data_journaling.is_none()
            && self.no_compress.is_none()
            && self.project_inherit.is_none()
            && self.secure_delete.is_none()
            && self.sync.is_none()
            && self.no_tail_merge.is_none()
            && self.top_dir.is_none()
            && self.undelete.is_none()
            && self.direct_access.is_none()
            && self.extent_format.is_none()
            && self.opaque.is_none()
            && self.win_attrs.is_none()
            && self.unix_flags.is_none()
    }

    #[cfg(windows)]
    pub fn from_win_attrs(attrs: u32) -> Self {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_ENCRYPTED,
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
            FILE_ATTRIBUTE_TEMPORARY,
        };

        Self {
            readonly: Some(attrs & FILE_ATTRIBUTE_READONLY != 0),
            hidden: Some(attrs & FILE_ATTRIBUTE_HIDDEN != 0),
            system: Some(attrs & FILE_ATTRIBUTE_SYSTEM != 0),
            archive: Some(attrs & FILE_ATTRIBUTE_ARCHIVE != 0),
            reparse_point: Some(attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0),
            compressed: Some(attrs & FILE_ATTRIBUTE_COMPRESSED != 0),
            encrypted: Some(attrs & FILE_ATTRIBUTE_ENCRYPTED != 0),
            temporary: Some(attrs & FILE_ATTRIBUTE_TEMPORARY != 0),
            offline: Some(attrs & FILE_ATTRIBUTE_OFFLINE != 0),
            not_content_indexed: Some(attrs & FILE_ATTRIBUTE_NOT_CONTENT_INDEXED != 0),
            win_attrs: Some(attrs),
            ..Self::default()
        }
    }

    #[cfg(target_os = "macos")]
    pub fn from_macos_flags(flags: u32) -> Self {
        use nix::sys::stat::FileFlag;

        let flags = FileFlag::from_bits_truncate(flags);

        Self {
            hidden: Some(flags.contains(FileFlag::UF_HIDDEN)),
            compressed: Some(flags.contains(FileFlag::UF_COMPRESSED)),
            immutable: Some(flags.contains(FileFlag::UF_IMMUTABLE)),
            append_only: Some(flags.contains(FileFlag::UF_APPEND)),
            no_dump: Some(flags.contains(FileFlag::UF_NODUMP)),
            opaque: Some(flags.contains(FileFlag::UF_OPAQUE)),
            unix_flags: Some(flags.bits()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WellKnownPath {
    HomeDir,
    CacheDir,
}

pub(crate) fn metadata_from_std(metadata: std::fs::Metadata) -> Metadata {
    #[cfg(unix)]
    {
        use nix::sys::stat::{SFlag, mode_t};
        #[cfg(target_os = "macos")]
        use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
        use std::os::unix::fs::MetadataExt;

        let mode = metadata.mode();
        let file_type = match SFlag::from_bits_truncate(mode as mode_t) & SFlag::S_IFMT {
            SFlag::S_IFREG => crate::FileType::File,
            SFlag::S_IFDIR => crate::FileType::Dir,
            SFlag::S_IFLNK => crate::FileType::Symlink,
            SFlag::S_IFIFO => crate::FileType::Fifo,
            SFlag::S_IFCHR => crate::FileType::CharacterDevice,
            SFlag::S_IFBLK => crate::FileType::BlockDevice,
            SFlag::S_IFSOCK => crate::FileType::Socket,
            _ => crate::FileType::Unknown,
        };

        Metadata {
            len: metadata.len(),
            file_type,
            atime: metadata.atime(),
            atime_nsec: metadata.atime_nsec(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
            mode,
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev(),
            blksize: metadata.blksize(),
            blocks: metadata.blocks(),
            #[cfg(target_os = "macos")]
            unix_flags: metadata.st_flags(),
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        let file_type = if metadata.is_file() {
            crate::FileType::File
        } else if metadata.is_dir() {
            crate::FileType::Dir
        } else if metadata.file_type().is_symlink() {
            crate::FileType::Symlink
        } else {
            crate::FileType::Unknown
        };

        Metadata {
            len: metadata.len(),
            file_type,
            atime: system_time_to_parts(metadata.accessed().ok()).0,
            atime_nsec: i64::from(system_time_to_parts(metadata.accessed().ok()).1),
            mtime: system_time_to_parts(metadata.modified().ok()).0,
            mtime_nsec: i64::from(system_time_to_parts(metadata.modified().ok()).1),
            ctime: system_time_to_parts(metadata.created().ok()).0,
            ctime_nsec: i64::from(system_time_to_parts(metadata.created().ok()).1),
            mode: if metadata.permissions().readonly() {
                0o444
            } else {
                0o666
            },
            dev: 0,
            ino: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 0,
            blocks: 0,
            win_attrs: metadata.file_attributes(),
        }
    }
}

#[cfg(windows)]
fn system_time_to_parts(time: Option<std::time::SystemTime>) -> (i64, u32) {
    use std::time::UNIX_EPOCH;

    let Some(time) = time else {
        return (0, 0);
    };

    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            duration.subsec_nanos(),
        ),
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            if duration.subsec_nanos() == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - duration.subsec_nanos())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChownIdentity {
    Id(u32),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    file_name: OsString,
    ino: u64,
    file_type: FileType,
}

impl DirEntry {
    pub fn file_name(&self) -> &std::ffi::OsStr {
        &self.file_name
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    pub fn file_type(&self) -> FileType {
        self.file_type
    }
}

pub use read_dir::ReadDir;

#[allow(async_fn_in_trait)]
pub trait OpenOptions {
    fn read(&mut self, read: bool) -> &mut Self;
    fn write(&mut self, write: bool) -> &mut Self;
    fn append(&mut self, append: bool) -> &mut Self;
    fn create(&mut self, create: bool) -> &mut Self;
    fn create_new(&mut self, create_new: bool) -> &mut Self;
    fn truncate(&mut self, truncate: bool) -> &mut Self;
    async fn open(&self, path: impl AsRef<Path>) -> Result<fs::File, io::Error>;
}

#[allow(async_fn_in_trait)]
pub trait Child {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error>;
    async fn terminate(self) -> Result<ExitStatus, io::Error>
    where
        Self: Sized;
}

#[allow(async_fn_in_trait)]
pub trait Command {
    type Child: Child;

    fn arg(&mut self, arg: &str) -> &mut Self;
    fn env(&mut self, key: &str, val: &str) -> &mut Self;
    fn env_remove(&mut self, key: &str) -> &mut Self;
    fn current_dir(&mut self, dir: &Path) -> &mut Self;
    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self>;
    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self>;
    fn stdin_inherit(&mut self) -> io::Result<&mut Self>;
    fn stdout_inherit(&mut self) -> io::Result<&mut Self>;
    #[cfg(unix)]
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self;
    #[cfg(unix)]
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self;
    fn stdin_null(&mut self) -> &mut Self;
    fn stdout_null(&mut self) -> &mut Self;
    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self>;
    fn stderr_inherit(&mut self) -> io::Result<&mut Self>;
    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self>;
    #[cfg(unix)]
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self;
    fn stderr_null(&mut self) -> &mut Self;
    async fn spawn(self) -> io::Result<Self::Child>;
}

#[allow(async_fn_in_trait)]
pub trait Vfs {
    type OpenOptions<'a>: OpenOptions
    where
        Self: 'a;
    type Command<'a>: Command
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_>;
    fn command(&self, program: impl AsRef<Path>) -> Self::Command<'_>;
    async fn read_dir(&self, path: impl AsRef<Path>) -> Result<ReadDir, io::Error>;
    async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<PathBuf>, io::Error>;
    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error>;
    async fn clear_cache(&self) -> Result<(), io::Error>;
    async fn file_metadata(&self, file: &fs::File) -> Result<Metadata, io::Error> {
        file.metadata().await.map(metadata_from_std)
    }

    async fn remove(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error>;
    async fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error>;
    async fn create_dir(&self, path: impl AsRef<Path>, all: bool) -> Result<(), io::Error>;
    async fn remove_dir(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error>;
    async fn copy(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error>;
    async fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), io::Error>;
    async fn move_(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error>;
    async fn symlink(&self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), io::Error>;
    async fn hard_link(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error>;
    async fn symlink_dir(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error>;
    async fn symlink_file(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error>;
    async fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error>;
    async fn attrs(&self, path: impl AsRef<Path>, follow: bool) -> Result<Attrs, io::Error>;
    async fn set_attrs(&self, path: impl AsRef<Path>, attrs: Attrs) -> Result<(), io::Error>;
    async fn canonicalize(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error>;
    async fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error>;
    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: &Path,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<PathBuf>, io::Error>;
    async fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: Permissions,
    ) -> Result<(), io::Error>;
    async fn utime(
        &self,
        path: impl AsRef<Path>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
    ) -> Result<(), io::Error>;
    async fn chown(
        &self,
        path: impl AsRef<Path>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error>;
}

pub use direct::{Direct, DirectOpenOptions};
pub use pipe::{PipeRecv, PipeSend, pipe};

#[cfg(unix)]
pub enum ClientOrDirectOpenOptions<'a> {
    Client(client::OpenOptions<'a>),
    Direct(DirectOpenOptions),
}

#[cfg(unix)]
impl OpenOptions for ClientOrDirectOpenOptions<'_> {
    fn read(&mut self, read: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.read(read);
            }
            Self::Direct(opts) => {
                opts.read(read);
            }
        }
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.write(write);
            }
            Self::Direct(opts) => {
                opts.write(write);
            }
        }
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.append(append);
            }
            Self::Direct(opts) => {
                opts.append(append);
            }
        }
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.create(create);
            }
            Self::Direct(opts) => {
                opts.create(create);
            }
        }
        self
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.create_new(create_new);
            }
            Self::Direct(opts) => {
                opts.create_new(create_new);
            }
        }
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        match self {
            Self::Client(opts) => {
                opts.truncate(truncate);
            }
            Self::Direct(opts) => {
                opts.truncate(truncate);
            }
        }
        self
    }

    async fn open(&self, path: impl AsRef<Path>) -> Result<fs::File, io::Error> {
        match self {
            Self::Client(opts) => opts.open(path).await,
            Self::Direct(opts) => opts.open(path).await,
        }
    }
}

#[cfg(not(unix))]
pub struct ClientOrDirectOpenOptions<'a> {
    inner: DirectOpenOptions,
    _marker: std::marker::PhantomData<&'a ()>,
}

#[cfg(not(unix))]
impl OpenOptions for ClientOrDirectOpenOptions<'_> {
    fn read(&mut self, read: bool) -> &mut Self {
        self.inner.read(read);
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        self.inner.write(write);
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        self.inner.append(append);
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        self.inner.create(create);
        self
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.inner.create_new(create_new);
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.inner.truncate(truncate);
        self
    }

    async fn open(&self, path: impl AsRef<Path>) -> Result<fs::File, io::Error> {
        self.inner.open(path).await
    }
}

#[cfg(unix)]
pub enum ClientOrDirectCommand<'a> {
    Client(client::CommandBuilder<'a>),
    Direct(direct::DirectCommand<'a>),
}

#[cfg(unix)]
pub enum ClientOrDirectChild<'a> {
    Client(client::ClientChild<'a>),
    Direct(direct::DirectChild),
}

#[cfg(unix)]
#[cfg(unix)]
impl Child for ClientOrDirectChild<'_> {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error> {
        match self {
            Self::Client(child) => child.wait().await,
            Self::Direct(child) => child.wait().await,
        }
    }

    async fn terminate(self) -> Result<ExitStatus, io::Error> {
        match self {
            Self::Client(child) => child.terminate().await,
            Self::Direct(child) => child.terminate().await,
        }
    }
}

#[cfg(unix)]
impl<'a> Command for ClientOrDirectCommand<'a> {
    type Child = ClientOrDirectChild<'a>;

    fn arg(&mut self, arg: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.arg(arg);
            }
            Self::Direct(builder) => {
                builder.arg(arg);
            }
        }
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.env(key, val);
            }
            Self::Direct(builder) => {
                builder.env(key, val);
            }
        }
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.env_remove(key);
            }
            Self::Direct(builder) => {
                builder.env_remove(key);
            }
        }
        self
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.current_dir(dir);
            }
            Self::Direct(builder) => {
                builder.current_dir(dir);
            }
        }
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdin_pipe(pipe)?;
            }
            Self::Direct(builder) => {
                builder.stdin_pipe(pipe)?;
            }
        }
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdout_pipe(pipe)?;
            }
            Self::Direct(builder) => {
                builder.stdout_pipe(pipe)?;
            }
        }
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdin_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stdin_inherit()?;
            }
        }
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stdout_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stdout_inherit()?;
            }
        }
        Ok(self)
    }

    #[cfg(unix)]
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdin_fd(fd);
            }
            Self::Direct(builder) => {
                builder.stdin_fd(fd);
            }
        }
        self
    }

    #[cfg(unix)]
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdout_fd(fd);
            }
            Self::Direct(builder) => {
                builder.stdout_fd(fd);
            }
        }
        self
    }

    fn stdin_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdin_null();
            }
            Self::Direct(builder) => {
                builder.stdin_null();
            }
        }
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stdout_null();
            }
            Self::Direct(builder) => {
                builder.stdout_null();
            }
        }
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr_pipe(pipe)?;
            }
            Self::Direct(builder) => {
                builder.stderr_pipe(pipe)?;
            }
        }
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr_inherit()?;
            }
            Self::Direct(builder) => {
                builder.stderr_inherit()?;
            }
        }
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        match self {
            Self::Client(builder) => {
                builder.stderr_inherit_stdout()?;
            }
            Self::Direct(builder) => {
                builder.stderr_inherit_stdout()?;
            }
        }
        Ok(self)
    }

    #[cfg(unix)]
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stderr_fd(fd);
            }
            Self::Direct(builder) => {
                builder.stderr_fd(fd);
            }
        }
        self
    }

    fn stderr_null(&mut self) -> &mut Self {
        match self {
            Self::Client(builder) => {
                builder.stderr_null();
            }
            Self::Direct(builder) => {
                builder.stderr_null();
            }
        }
        self
    }

    async fn spawn(self) -> io::Result<Self::Child> {
        match self {
            Self::Client(builder) => builder.spawn().await.map(ClientOrDirectChild::Client),
            Self::Direct(builder) => builder.spawn().await.map(ClientOrDirectChild::Direct),
        }
    }
}

#[cfg(not(unix))]
pub struct ClientOrDirectCommand<'a> {
    inner: direct::DirectCommand<'a>,
    _marker: std::marker::PhantomData<&'a ()>,
}

#[cfg(not(unix))]
pub struct ClientOrDirectChild<'a> {
    inner: direct::DirectChild,
    _marker: std::marker::PhantomData<&'a ()>,
}

#[cfg(not(unix))]
#[cfg(not(unix))]
impl Child for ClientOrDirectChild<'_> {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error> {
        self.inner.wait().await
    }

    async fn terminate(self) -> Result<ExitStatus, io::Error> {
        self.inner.terminate().await
    }
}

#[cfg(not(unix))]
impl<'a> Command for ClientOrDirectCommand<'a> {
    type Child = ClientOrDirectChild<'a>;

    fn arg(&mut self, arg: &str) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.inner.env(key, val);
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.inner.stdin_pipe(pipe)?;
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.inner.stdout_pipe(pipe)?;
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.inner.stdin_inherit()?;
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.inner.stdout_inherit()?;
        Ok(self)
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.inner.stdin_null();
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.inner.stdout_null();
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.inner.stderr_pipe(pipe)?;
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.inner.stderr_inherit()?;
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.inner.stderr_inherit_stdout()?;
        Ok(self)
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.inner.stderr_null();
        self
    }

    async fn spawn(self) -> io::Result<Self::Child> {
        Ok(ClientOrDirectChild {
            inner: self.inner.spawn().await?,
            _marker: std::marker::PhantomData,
        })
    }
}

#[cfg(unix)]
#[derive(Clone)]
pub enum ClientOrDirect {
    Client(client::Client),
    Direct(Direct),
}

#[cfg(not(unix))]
#[derive(Clone, Default)]
pub struct ClientOrDirect(Direct);

#[cfg(unix)]
impl Default for ClientOrDirect {
    fn default() -> Self {
        Self::Direct(Direct::default())
    }
}

#[cfg(unix)]
impl From<client::Client> for ClientOrDirect {
    fn from(value: client::Client) -> Self {
        Self::Client(value)
    }
}

impl From<Direct> for ClientOrDirect {
    fn from(value: Direct) -> Self {
        #[cfg(unix)]
        {
            Self::Direct(value)
        }
        #[cfg(not(unix))]
        {
            Self(value)
        }
    }
}

#[cfg(unix)]
impl ClientOrDirect {
    pub fn as_client(&self) -> Option<&client::Client> {
        match self {
            Self::Client(client) => Some(client),
            Self::Direct(_) => None,
        }
    }

    pub fn into_client(self) -> Option<client::Client> {
        match self {
            Self::Client(client) => Some(client),
            Self::Direct(_) => None,
        }
    }
}

impl Vfs for ClientOrDirect {
    type OpenOptions<'a>
        = ClientOrDirectOpenOptions<'a>
    where
        Self: 'a;
    type Command<'a>
        = ClientOrDirectCommand<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => ClientOrDirectOpenOptions::Client(client.open_options()),
                Self::Direct(direct) => ClientOrDirectOpenOptions::Direct(direct.open_options()),
            }
        }
        #[cfg(not(unix))]
        {
            ClientOrDirectOpenOptions {
                inner: self.0.open_options(),
                _marker: std::marker::PhantomData,
            }
        }
    }

    fn command(&self, program: impl AsRef<Path>) -> Self::Command<'_> {
        let program = program.as_ref().to_path_buf();
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => ClientOrDirectCommand::Client(client.command(&program)),
                Self::Direct(direct) => ClientOrDirectCommand::Direct(direct.command(&program)),
            }
        }
        #[cfg(not(unix))]
        {
            ClientOrDirectCommand {
                inner: self.0.command(&program),
                _marker: std::marker::PhantomData,
            }
        }
    }

    async fn read_dir(&self, path: impl AsRef<Path>) -> Result<ReadDir, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.read_dir(path).await,
                Self::Direct(direct) => direct.read_dir(path).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.read_dir(path).await
        }
    }

    async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<PathBuf>, io::Error> {
        let program = program.as_ref().to_path_buf();
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.which(&program, path, cwd).await,
                Self::Direct(direct) => direct.which(&program, path, cwd).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.which(&program, path, cwd).await
        }
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.well_known_path(key, env).await,
                Self::Direct(direct) => direct.well_known_path(key, env).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.well_known_path(key, env).await
        }
    }

    async fn clear_cache(&self) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.clear_cache().await,
                Self::Direct(direct) => direct.clear_cache().await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.clear_cache().await
        }
    }

    async fn remove(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.remove(path, all, ignore).await,
                Self::Direct(direct) => direct.remove(path, all, ignore).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.remove(path, all, ignore).await
        }
    }

    async fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.metadata(path).await,
                Self::Direct(direct) => direct.metadata(path).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.metadata(path).await
        }
    }

    async fn create_dir(&self, path: impl AsRef<Path>, all: bool) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.create_dir(path, all).await,
                Self::Direct(direct) => direct.create_dir(path, all).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.create_dir(path, all).await
        }
    }

    async fn remove_dir(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.remove_dir(path, all, ignore).await,
                Self::Direct(direct) => direct.remove_dir(path, all, ignore).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.remove_dir(path, all, ignore).await
        }
    }

    async fn copy(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.copy(from, to, all).await,
                Self::Direct(direct) => direct.copy(from, to, all).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.copy(from, to, all).await
        }
    }

    async fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.rename(from, to).await,
                Self::Direct(direct) => direct.rename(from, to).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.rename(from, to).await
        }
    }

    async fn move_(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.move_(from, to, all).await,
                Self::Direct(direct) => direct.move_(from, to, all).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.move_(from, to, all).await
        }
    }

    async fn symlink(&self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.symlink(src, dst).await,
                Self::Direct(direct) => direct.symlink(src, dst).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.symlink(src, dst).await
        }
    }

    async fn hard_link(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.hard_link(src, dst).await,
                Self::Direct(direct) => direct.hard_link(src, dst).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.hard_link(src, dst).await
        }
    }

    async fn symlink_dir(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.symlink_dir(src, dst).await,
                Self::Direct(direct) => direct.symlink_dir(src, dst).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.symlink_dir(src, dst).await
        }
    }

    async fn symlink_file(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.symlink_file(src, dst).await,
                Self::Direct(direct) => direct.symlink_file(src, dst).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.symlink_file(src, dst).await
        }
    }

    async fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.symlink_metadata(path).await,
                Self::Direct(direct) => direct.symlink_metadata(path).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.symlink_metadata(path).await
        }
    }

    async fn attrs(&self, path: impl AsRef<Path>, follow: bool) -> Result<Attrs, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.attrs(path, follow).await,
                Self::Direct(direct) => direct.attrs(path, follow).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.attrs(path, follow).await
        }
    }

    async fn set_attrs(&self, path: impl AsRef<Path>, attrs: Attrs) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.set_attrs(path, attrs).await,
                Self::Direct(direct) => direct.set_attrs(path, attrs).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.set_attrs(path, attrs).await
        }
    }

    async fn canonicalize(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.canonicalize(path).await,
                Self::Direct(direct) => direct.canonicalize(path).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.canonicalize(path).await
        }
    }

    async fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.read_link(path).await,
                Self::Direct(direct) => direct.read_link(path).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.read_link(path).await
        }
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: &Path,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<PathBuf>, io::Error> {
        let pattern = pattern.into();
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => {
                    client.glob(pattern, root, follow_symlinks, max_depth).await
                }
                Self::Direct(direct) => {
                    direct.glob(pattern, root, follow_symlinks, max_depth).await
                }
            }
        }
        #[cfg(not(unix))]
        {
            self.0.glob(pattern, root, follow_symlinks, max_depth).await
        }
    }

    async fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: Permissions,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.set_permissions(path, perm).await,
                Self::Direct(direct) => direct.set_permissions(path, perm).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.set_permissions(path, perm).await
        }
    }

    async fn utime(
        &self,
        path: impl AsRef<Path>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.utime(path, accessed, modified).await,
                Self::Direct(direct) => direct.utime(path, accessed, modified).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.utime(path, accessed, modified).await
        }
    }

    async fn chown(
        &self,
        path: impl AsRef<Path>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match self {
                Self::Client(client) => client.chown(path, user, group, follow).await,
                Self::Direct(direct) => direct.chown(path, user, group, follow).await,
            }
        }
        #[cfg(not(unix))]
        {
            self.0.chown(path, user, group, follow).await
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    /// Client for connecting to the agent daemon and spawning processes.
    pub use client::Client;
    /// Builder for constructing spawn requests.
    pub use client::CommandBuilder;
    /// Query result containing the daemon's environment and working directory.
    pub use client::Query;
    /// Access permission flags for the `access` method.
    pub use nix::unistd::AccessFlags;
    /// Agent server that accepts connections and handles spawn requests.
    pub use server::Server;
    /// Daemonization errors.
    pub use service::ServiceError;
    /// Run the agent server in foreground mode (no daemonization).
    pub use service::foreground;

    /// Thread-safe wrapper for `tokio_unix_ipc::Sender`.
    ///
    /// The underlying sender is `Sync` but will corrupt messages with concurrent sends.
    /// This wrapper serializes access via a mutex to ensure message integrity.
    pub(crate) struct LockedSender<T>(pub(crate) tokio::sync::Mutex<tokio_unix_ipc::Sender<T>>);

    impl<T: serde::Serialize + for<'de> serde::Deserialize<'de>> LockedSender<T> {
        pub(crate) fn new(sender: tokio_unix_ipc::Sender<T>) -> Self {
            Self(tokio::sync::Mutex::new(sender))
        }

        pub(crate) async fn send(&self, message: T) -> std::io::Result<()> {
            self.0.lock().await.send(message).await
        }
    }
}

#[cfg(unix)]
pub use unix::*;
