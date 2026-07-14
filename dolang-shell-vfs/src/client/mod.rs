use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitStatus,
    task::{Context, Poll},
};

#[cfg(unix)]
use std::os::unix::{
    io::{AsFd, OwnedFd},
    net::UnixStream as StdUnixStream,
    process::ExitStatusExt,
};
#[cfg(windows)]
use std::os::windows::{
    io::{AsHandle, OwnedHandle},
    process::ExitStatusExt,
};

use dolang_rpc::{Call, DefaultHandle, OsHandle};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
use crate::protocol::{AccessRequest, UnixStreamSocketRequest};
use crate::{
    Attrs, Child, ChownIdentity, Command, FileHandle, FsMetadata, Metadata, Permissions, PipeRecv,
    PipeSend, Query, ReadDir, StreamEntry, Utf8TypedPath, Utf8TypedPathBuf, Vfs, WellKnownPath,
    XattrEntry,
    direct::DirectFile,
    protocol::{
        AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest, CreateDirRequest,
        FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest, MoveRequest, OpenRequest,
        ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest, RequestKind, ResponseKind,
        SetAttrsRequest, SetPermissionsRequest, SetTimesRequest, SetXattrRequest, SpawnRequest,
        StreamsRequest, SymlinkKind, SymlinkRequest, Timestamp, VfsProtocol, WellKnownPathRequest,
        WirePath, XattrRequest, XattrsRequest,
    },
};

/// Client for connecting to the agent daemon and spawning processes.
#[derive(Clone)]
pub struct Client {
    rpc: dolang_rpc::Client<VfsProtocol>,
}

#[derive(Debug)]
pub struct ClientFile(DirectFile);

impl ClientFile {
    fn from_std(file: std::fs::File) -> Self {
        Self(DirectFile::from_std(file))
    }
}

impl AsyncRead for ClientFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for ClientFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl AsyncSeek for ClientFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        Pin::new(&mut self.0).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.0).poll_complete(cx)
    }
}

impl FileHandle for ClientFile {
    async fn try_clone(&self) -> crate::Result<Self> {
        self.0.try_clone().await.map(Self)
    }

    async fn close(self) -> crate::Result<()> {
        self.0.close().await
    }

    async fn set_len(&mut self, size: u64) -> crate::Result<()> {
        self.0.set_len(size).await
    }

    async fn metadata(&mut self) -> crate::Result<Metadata> {
        self.0.metadata().await
    }

    async fn fs_metadata(&mut self) -> crate::Result<FsMetadata> {
        self.0.fs_metadata().await
    }

    async fn xattrs(
        &mut self,
        namespace: crate::XattrNamespace<'_>,
    ) -> crate::Result<Vec<XattrEntry>> {
        self.0.xattrs(namespace).await
    }

    async fn xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<Vec<u8>> {
        self.0.xattr(name, namespace).await
    }

    async fn streams(&mut self) -> crate::Result<Vec<StreamEntry>> {
        self.0.streams().await
    }

    async fn set_xattr(
        &mut self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> crate::Result<()> {
        self.0.set_xattr(name, namespace, value).await
    }

    async fn remove_xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<()> {
        self.0.remove_xattr(name, namespace).await
    }

    async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        self.0.try_into_std().await.map_err(Self)
    }
}

impl Client {
    /// Connect to an agent daemon at the given socket path.
    #[cfg(unix)]
    pub async fn connect(path: impl AsRef<Path>) -> crate::Result<Self> {
        Self::from_stream(UnixStream::connect(path).await?).await
    }

    /// Connect using an existing `UnixStream`.
    #[cfg(unix)]
    pub async fn from_stream(stream: UnixStream) -> crate::Result<Self> {
        Self::from_std_stream(stream.into_std()?)
    }

    #[cfg(unix)]
    fn from_std_stream(stream: StdUnixStream) -> crate::Result<Self> {
        let rpc = dolang_rpc::Client::from_unix_stream(stream)?;
        Ok(Self { rpc })
    }

    /// Starts a VFS client on the server end of a connected Windows named pipe.
    ///
    /// # Safety
    ///
    /// `server_process` must identify the trusted process at the other end of
    /// the pipe. That process can transfer handles which this process adopts.
    #[cfg(windows)]
    pub unsafe fn from_named_pipe_server(
        pipe: NamedPipeServer,
        server_process: OwnedHandle,
    ) -> crate::Result<Self> {
        let rpc = unsafe { dolang_rpc::Client::from_named_pipe_server(pipe, server_process)? };
        Ok(Self { rpc })
    }

    fn call(&self, request: RequestKind) -> Call<ResponseKind> {
        self.rpc.call(request)
    }

    async fn request(&self, request: RequestKind) -> crate::Result<ResponseKind> {
        self.call(request)
            .await
            .map_err(rpc_error)
            .map_err(Into::into)
    }

    /// Create a Unix stream socket in the agent namespace and return its descriptor.
    ///
    /// If `bind` is provided, the socket is bound to that pathname first. If `connect`
    /// is provided, the socket is then connected to that pathname. Either or both may
    /// be omitted.
    #[cfg(unix)]
    pub async fn unix_stream_socket<B, C>(
        &self,
        bind: Option<B>,
        connect: Option<C>,
    ) -> crate::Result<OwnedFd>
    where
        B: AsRef<Path>,
        C: AsRef<Path>,
    {
        let req = UnixStreamSocketRequest {
            bind: bind
                .map(|p| WirePath::try_from(p.as_ref().to_path_buf()))
                .transpose()?,
            connect: connect
                .map(|p| WirePath::try_from(p.as_ref().to_path_buf()))
                .transpose()?,
        };

        match self.request(RequestKind::UnixStreamSocket(req)).await? {
            ResponseKind::UnixStreamSocket(result) => {
                result.map(OsHandle::into_inner).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }

    /// Check file accessibility.
    ///
    /// Mode is a bitmask of accessibility flags from [`AccessFlags`](crate::AccessFlags):
    /// - `AccessFlags::F_OK`: Test for existence
    /// - `AccessFlags::R_OK`: Test for read permission
    /// - `AccessFlags::W_OK`: Test for write permission
    /// - `AccessFlags::X_OK`: Test for execute permission
    #[cfg(unix)]
    pub async fn access(
        &self,
        path: impl AsRef<Path>,
        mode: crate::AccessFlags,
    ) -> crate::Result<()> {
        let request = AccessRequest {
            path: path.as_ref().to_path_buf().try_into()?,
            mode: mode.bits(),
        };
        match self.request(RequestKind::Access(request)).await? {
            ResponseKind::Access(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    /// Query the daemon's environment variables and current working directory.
    pub async fn query(&self) -> crate::Result<Query> {
        match self.request(RequestKind::Query).await? {
            ResponseKind::Query { env, cwd, target } => Ok(Query {
                env,
                cwd: cwd.into(),
                target,
            }),
            response => Err(unexpected(response).into()),
        }
    }

    /// Resolve a program path using the daemon's PATH resolution.
    pub async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> crate::Result<Option<PathBuf>> {
        let request = RequestKind::Which {
            program: program.as_ref().to_path_buf().try_into()?,
            path: path.map(str::to_owned),
            cwd: cwd
                .map(|path| WirePath::try_from(path.to_path_buf()))
                .transpose()?,
        };
        match self.request(request).await? {
            ResponseKind::Which(result) => result.map(TryInto::try_into).transpose(),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> crate::Result<PathBuf> {
        let request = WellKnownPathRequest {
            key,
            env: env.clone(),
        };
        match self.request(RequestKind::WellKnownPath(request)).await? {
            ResponseKind::WellKnownPath(result) => result.map_err(crate::Error::from)?.try_into(),
            response => Err(unexpected(response).into()),
        }
    }

    /// Signal the daemon to stop accepting new connections.
    pub async fn stop(&self) -> crate::Result<()> {
        match self.request(RequestKind::Stop).await? {
            ResponseKind::Stop => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    /// Clear the server's path resolution cache.
    pub async fn clear_cache(&self) -> crate::Result<()> {
        match self.request(RequestKind::ClearCache).await? {
            ResponseKind::ClearCache => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }
}

#[cfg(unix)]
impl TryFrom<OwnedFd> for Client {
    type Error = crate::Error;

    fn try_from(value: OwnedFd) -> Result<Self, Self::Error> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Self::from_std_stream(stream)
    }
}

fn rpc_error(error: dolang_rpc::Error) -> io::Error {
    match error {
        dolang_rpc::Error::Io(error) => error,
        dolang_rpc::Error::ConnectionClosed => {
            io::Error::new(io::ErrorKind::ConnectionReset, error.to_string())
        }
        dolang_rpc::Error::Cancelled => {
            io::Error::new(io::ErrorKind::Interrupted, error.to_string())
        }
        error => io::Error::other(error),
    }
}

fn unexpected(response: ResponseKind) -> io::Error {
    io::Error::other(format!("unexpected RPC response: {response:?}"))
}

#[cfg(unix)]
fn exit_status_from_raw(raw: i32) -> ExitStatus {
    ExitStatus::from_raw(raw)
}

#[cfg(windows)]
fn exit_status_from_raw(raw: i32) -> ExitStatus {
    ExitStatus::from_raw(raw as u32)
}

fn clone_stdin_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stdin().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stdin().as_handle().try_clone_to_owned()
    }
}

fn clone_stdout_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stdout().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stdout().as_handle().try_clone_to_owned()
    }
}

fn clone_stderr_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stderr().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stderr().as_handle().try_clone_to_owned()
    }
}

/// Builder for constructing spawn requests.
///
/// # Example
///
/// ```ignore
/// let status = client
///     .command("ls")
///     .arg("-l")
///     .arg("/tmp")
///     .env("RUST_LOG", "info")
///     .env_remove("DEBUG")
///     .current_dir("/home")
///     .stdin(fd)
///     .spawn()
///     .await?;
/// ```
pub struct CommandBuilder<'a> {
    client: &'a Client,
    program: WirePath,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<WirePath>,
    stdin_handle: Option<DefaultHandle>,
    stdout_handle: Option<DefaultHandle>,
    stderr_handle: Option<DefaultHandle>,
    stdin_file: Option<ClientFile>,
    stdout_file: Option<ClientFile>,
    stderr_file: Option<ClientFile>,
}

pub struct ClientChild<'a> {
    inner: Option<Call<ResponseKind>>,
    marker: std::marker::PhantomData<&'a Client>,
}

impl<'a> CommandBuilder<'a> {
    fn new(client: &'a Client, program: Utf8TypedPath<'_>) -> Self {
        Self {
            client,
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin_handle: None,
            stdout_handle: None,
            stderr_handle: None,
            stdin_file: None,
            stdout_file: None,
            stderr_file: None,
        }
    }
}

impl Child for ClientChild<'_> {
    async fn wait(&mut self) -> crate::Result<ExitStatus> {
        let result = match self.inner.take() {
            Some(inner) => inner.await.map_err(rpc_error).map_err(crate::Error::from),
            None => return Err(io::Error::other("child already waited").into()),
        }?;
        match result {
            ResponseKind::Spawn(result) => {
                result.map(exit_status_from_raw).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn terminate(self) -> crate::Result<ExitStatus> {
        let Some(mut inner) = self.inner else {
            return Err(io::Error::other("child already waited").into());
        };
        inner.cancel();
        match inner.await.map_err(rpc_error).map_err(crate::Error::from)? {
            ResponseKind::Spawn(result) => {
                result.map(exit_status_from_raw).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }
}

impl<'a> Command for CommandBuilder<'a> {
    type Child = ClientChild<'a>;
    type File = ClientFile;
    type PipeSend = PipeSend;
    type PipeRecv = PipeRecv;

    fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.stdin_file = None;
        self.stdin_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stdout_file = None;
        self.stdout_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin_file = None;
        self.stdin_handle = Some(clone_stdin_handle()?);
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout_file = None;
        self.stdout_handle = Some(clone_stdout_handle()?);
        Ok(self)
    }

    fn stdin_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stdin_handle = None;
        self.stdin_file = Some(handle);
        Ok(self)
    }

    fn stdout_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stdout_handle = None;
        self.stdout_file = Some(handle);
        Ok(self)
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin_file = None;
        self.stdin_handle = None;
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout_file = None;
        self.stdout_handle = None;
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(clone_stderr_handle()?);
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(clone_stdout_handle()?);
        Ok(self)
    }

    fn stderr_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stderr_handle = None;
        self.stderr_file = Some(handle);
        Ok(self)
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr_file = None;
        self.stderr_handle = None;
        self
    }

    async fn spawn(mut self) -> crate::Result<Self::Child> {
        if let Some(file) = self.stdin_file.take() {
            self.stdin_handle = Some(file.0.try_into_std().await.unwrap().into());
        }
        if let Some(file) = self.stdout_file.take() {
            self.stdout_handle = Some(file.0.try_into_std().await.unwrap().into());
        }
        if let Some(file) = self.stderr_file.take() {
            self.stderr_handle = Some(file.0.try_into_std().await.unwrap().into());
        }
        let req = SpawnRequest {
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            stdin_fd: self.stdin_handle.map(OsHandle::new),
            stdout_fd: self.stdout_handle.map(OsHandle::new),
            stderr_fd: self.stderr_handle.map(OsHandle::new),
        };
        Ok(ClientChild {
            inner: Some(self.client.call(RequestKind::Spawn(req))),
            marker: std::marker::PhantomData,
        })
    }
}

/// Builder for opening files with configurable options.
///
/// # Example
///
/// ```ignore
/// let file = client
///     .open_options()
///     .read(true)
///     .write(true)
///     .create(true)
///     .open("/tmp/myfile.txt")
///     .await?;
/// ```
pub struct OpenOptions<'a> {
    client: &'a Client,
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    no_follow: bool,
}

impl<'a> OpenOptions<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            read: false,
            write: false,
            append: false,
            create: false,
            create_new: false,
            truncate: false,
            no_follow: false,
        }
    }

    /// Set read access mode.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Set write access mode.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Set append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Set create mode (creates file if it doesn't exist).
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Set create_new mode (fails if file already exists).
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Set truncate mode (truncates file on open).
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Set no-follow mode for the final path component.
    pub fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    async fn open_wire(&self, path: WirePath) -> crate::Result<ClientFile> {
        let req = OpenRequest {
            path,
            read: self.read,
            write: self.write,
            append: self.append,
            create: self.create,
            create_new: self.create_new,
            truncate: self.truncate,
            no_follow: self.no_follow,
        };

        match self.client.request(RequestKind::Open(req)).await? {
            ResponseKind::Open(result) => result
                .map(|fd| ClientFile::from_std(fd.into_inner().into()))
                .map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    /// Open the file at the given path.
    pub async fn open(&self, path: impl AsRef<Path>) -> crate::Result<ClientFile> {
        self.open_wire(path.as_ref().to_path_buf().try_into()?)
            .await
    }
}

impl crate::OpenOptions for OpenOptions<'_> {
    type File = ClientFile;

    fn read(&mut self, read: bool) -> &mut Self {
        self.read(read)
    }

    fn write(&mut self, write: bool) -> &mut Self {
        self.write(write)
    }

    fn append(&mut self, append: bool) -> &mut Self {
        self.append(append)
    }

    fn create(&mut self, create: bool) -> &mut Self {
        self.create(create)
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new(create_new)
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate(truncate)
    }

    fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow(no_follow)
    }

    async fn open(&self, path: Utf8TypedPath<'_>) -> crate::Result<ClientFile> {
        self.open_wire(path.into()).await
    }
}

impl Vfs for Client {
    type File = ClientFile;
    type PipeSend = PipeSend;
    type PipeRecv = PipeRecv;
    type OpenOptions<'a>
        = OpenOptions<'a>
    where
        Self: 'a;
    type Command<'a>
        = CommandBuilder<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        OpenOptions::new(self)
    }

    fn command(&self, program: Utf8TypedPath<'_>) -> Self::Command<'_> {
        CommandBuilder::new(self, program)
    }

    fn pipe(&self) -> io::Result<(PipeSend, PipeRecv)> {
        crate::pipe::pipe()
    }

    async fn query(&self) -> crate::Result<Query> {
        Client::query(self).await
    }

    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> crate::Result<ReadDir> {
        match self
            .request(RequestKind::ReadDir { path: path.into() })
            .await?
        {
            ResponseKind::ReadDir(result) => result
                .map(ReadDir::from_entries)
                .map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> crate::Result<Option<Utf8TypedPathBuf>> {
        let request = RequestKind::Which {
            program: program.into(),
            path: path.map(str::to_owned),
            cwd: cwd.map(Into::into),
        };
        match self.request(request).await? {
            ResponseKind::Which(result) => Ok(result.map(Into::into)),
            response => Err(unexpected(response).into()),
        }
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> crate::Result<Utf8TypedPathBuf> {
        let request = WellKnownPathRequest {
            key,
            env: env.clone(),
        };
        match self.request(RequestKind::WellKnownPath(request)).await? {
            ResponseKind::WellKnownPath(result) => {
                result.map(Into::into).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn clear_cache(&self) -> crate::Result<()> {
        Client::clear_cache(self).await
    }

    async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: crate::XattrNamespace<'_>,
        follow: bool,
    ) -> crate::Result<Vec<XattrEntry>> {
        let request = XattrsRequest {
            path: path.into(),
            namespace: namespace.into(),
            follow,
        };
        match self.request(RequestKind::Xattrs(request)).await? {
            ResponseKind::Xattrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn streams(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<Vec<StreamEntry>> {
        let request = StreamsRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::Streams(request)).await? {
            ResponseKind::Streams(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<Vec<u8>> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::Xattr(request)).await? {
            ResponseKind::Xattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> crate::Result<()> {
        let request = SetXattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            value: value.to_vec(),
            follow,
        };
        match self.request(RequestKind::SetXattr(request)).await? {
            ResponseKind::SetXattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<()> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::RemoveXattr(request)).await? {
            ResponseKind::RemoveXattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> crate::Result<()> {
        let request = RemoveRequest {
            path: path.into(),
            all,
            ignore,
        };
        match self.request(RequestKind::Remove(request)).await? {
            ResponseKind::Remove(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::Metadata(request)).await? {
            ResponseKind::Metadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn fs_metadata(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<FsMetadata> {
        let request = FsMetadataRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::FsMetadata(request)).await? {
            ResponseKind::FsMetadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> crate::Result<()> {
        let request = CreateDirRequest {
            path: path.into(),
            all,
        };
        match self.request(RequestKind::CreateDir(request)).await? {
            ResponseKind::CreateDir(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove_dir(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> crate::Result<()> {
        let request = RemoveDirRequest {
            path: path.into(),
            ignore,
            all,
        };
        match self.request(RequestKind::RemoveDir(request)).await? {
            ResponseKind::RemoveDir(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        let request = CopyRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Copy(request)).await? {
            ResponseKind::Copy(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn rename(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>) -> crate::Result<()> {
        let request = RenameRequest {
            from: from.into(),
            to: to.into(),
        };
        match self.request(RequestKind::Rename(request)).await? {
            ResponseKind::Rename(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        let request = MoveRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Move(request)).await? {
            ResponseKind::Move(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: cwd.into(),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Infer,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn hard_link(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> crate::Result<()> {
        let request = HardLinkRequest {
            src: src.into(),
            dst: dst.into(),
        };
        match self.request(RequestKind::HardLink(request)).await? {
            ResponseKind::HardLink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_dir(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Dir,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_file(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::File,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::SymlinkMetadata(request)).await? {
            ResponseKind::SymlinkMetadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn attrs(&self, path: Utf8TypedPath<'_>, follow: bool) -> crate::Result<Attrs> {
        let request = AttrsRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::Attrs(request)).await? {
            ResponseKind::Attrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_attrs(&self, path: Utf8TypedPath<'_>, attrs: Attrs) -> crate::Result<()> {
        let request = SetAttrsRequest {
            path: path.into(),
            attrs,
        };
        match self.request(RequestKind::SetAttrs(request)).await? {
            ResponseKind::SetAttrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        let request = CanonicalizeRequest { path: path.into() };
        match self.request(RequestKind::Canonicalize(request)).await? {
            ResponseKind::Canonicalize(result) => {
                result.map_err(crate::Error::from).map(Into::into)
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn read_link(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        let request = ReadLinkRequest { path: path.into() };
        match self.request(RequestKind::ReadLink(request)).await? {
            ResponseKind::ReadLink(result) => result.map_err(crate::Error::from).map(Into::into),
            response => Err(unexpected(response).into()),
        }
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> crate::Result<Vec<Utf8TypedPathBuf>> {
        let request = GlobRequest {
            pattern: pattern.into(),
            root: root.into(),
            follow_symlinks,
            max_depth,
        };
        match self.request(RequestKind::Glob(request)).await? {
            ResponseKind::Glob(result) => Ok(result
                .map_err(crate::Error::from)?
                .into_iter()
                .map(Utf8TypedPathBuf::from)
                .collect()),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_permissions(
        &self,
        path: Utf8TypedPath<'_>,
        perm: Permissions,
    ) -> crate::Result<()> {
        let request = SetPermissionsRequest {
            path: path.into(),
            mode: perm.mode(),
        };
        match self.request(RequestKind::SetPermissions(request)).await? {
            ResponseKind::SetPermissions(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_times(
        &self,
        path: Utf8TypedPath<'_>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> crate::Result<()> {
        let request = SetTimesRequest {
            path: path.into(),
            accessed: accessed.map(|(secs, nanos)| Timestamp { secs, nanos }),
            modified: modified.map(|(secs, nanos)| Timestamp { secs, nanos }),
            created: created.map(|(secs, nanos)| Timestamp { secs, nanos }),
        };
        match self.request(RequestKind::SetTimes(request)).await? {
            ResponseKind::SetTimes(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn chown(
        &self,
        path: Utf8TypedPath<'_>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> crate::Result<()> {
        let request = ChownRequest {
            path: path.into(),
            user,
            group,
            follow,
        };
        match self.request(RequestKind::Chown(request)).await? {
            ResponseKind::Chown(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }
}
