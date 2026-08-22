#![deny(warnings)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//! Filesystem and process operations over either a local or remote target.
//!
//! [`Vfs`] performs operations in either the current process's environment
//! or through a `dolang-vfs` agent.
//!
//! Paths passed through [`Vfs`] are `typed_path::Utf8TypedPath` values. Their syntax
//! belongs to the target VFS rather than necessarily to the host running this
//! code, which lets a Unix host describe Windows paths and vice versa.

use dolang_winterop::security::{SecDesc, Sid};
use extension::VfsExtension;
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite};
/// Remote VFS client implementation.
mod client;
/// Local-process VFS implementation.
mod direct;
/// Directory iteration types.
pub mod directory;
/// Error types returned by VFS operations.
pub mod error;
pub mod extension;
pub mod file;
mod macos_acl;
pub mod metadata;
mod nfs4_acl;
pub mod path;
mod posix_acl;
/// Process status, control, and standard-I/O types.
pub mod process;
mod protocol;
pub mod security;
/// RPC server implementation.
pub mod server;
mod session;
pub mod target;
#[cfg(windows)]
mod windows;

/// Buffer size used when pumping bulk byte streams (stdio relays, file and
/// stdio trailer transfers).
///
/// Each remote read or write turns into one round trip, so the buffer size
/// sets how much data one round trip carries. `tokio::io::copy`'s built-in
/// 8 KiB buffer makes that ratio disastrous for streaming; the copies here use
/// `copy_buf` over a buffer of this size instead. It matches the default
/// `dolang-rpc` maximum fragment size and the runtime's
/// `BYTE_STREAM_CHUNK_SIZE`, so a full buffer maps onto a single wire
/// fragment, and it also amortizes syscalls on purely local transfers.
const STREAM_CHUNK_SIZE: usize = 512 * 1024;

/// Largest range one `FileRead` request may ask for.
///
/// The server reads the whole requested range into memory *before* it responds,
/// so that a filesystem error becomes a structured failure in the response
/// rather than an aborted trailer with the `ErrorKind` lost. That makes the
/// requested length an allocation the peer controls, so it has to be bounded.
/// One chunk keeps a full reply inside a single wire fragment, matching what
/// the trailer pool already budgets per transfer.
///
/// Reads larger than this are not an error: both the request length and the
/// reply are clamped, and the caller sees an ordinary short read. Callers that
/// must have every byte loop, exactly as they already must around a short read
/// at any other layer.
const MAX_FILE_READ: usize = STREAM_CHUNK_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionMode {
    Native,
    Remote,
}

/// A filesystem and process-execution backend backed by either a remote client
/// or the local process.
///
/// Path arguments always use the target's syntax; consult [`Vfs::target`] when
/// selecting one for a remote VFS.
#[derive(Clone)]
enum VfsInner {
    Client(client::Client),
    Direct(direct::Direct),
}

#[derive(Clone)]
pub struct Vfs {
    inner: VfsInner,
}

impl Vfs {
    fn from_client(client: client::Client) -> Self {
        Self {
            inner: VfsInner::Client(client),
        }
    }

    fn from_direct(direct: direct::Direct) -> Self {
        Self {
            inner: VfsInner::Direct(direct),
        }
    }

    /// Creates a VFS that accesses the local process directly.
    pub fn direct() -> error::Result<Self> {
        direct::Direct::new().map(Self::from_direct)
    }

    /// Starts an opaque-only VFS over a bidirectional byte stream.
    ///
    /// This transport cannot transfer native handles, so files, subprocesses,
    /// and stdio endpoints are represented by remote references and relays.
    pub async fn new<T>(stream: T) -> error::Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        client::Client::new(stream).await.map(Self::from_client)
    }

    /// Starts an opaque-only VFS on separate reader and writer streams.
    ///
    /// This has the same opaque-only behavior as [`new`](Self::new).
    pub async fn new_split<R, W>(reader: R, writer: W) -> error::Result<Self>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        client::Client::new_split(reader, writer)
            .await
            .map(Self::from_client)
    }

    /// Connects to an agent daemon at a Unix-domain socket path.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub async fn connect(path: impl AsRef<std::path::Path>) -> error::Result<Self> {
        client::Client::connect(path).await.map(Self::from_client)
    }

    /// Connects to an agent daemon at a Unix-domain socket path, proving
    /// knowledge of a pre-shared key.
    ///
    /// A socket that must be world-connectable cannot identify its peer from
    /// credentials alone, so `key` distinguishes the intended agent and
    /// client. Both ends must agree on the key.
    #[cfg(unix)]
    pub async fn connect_with_key(
        path: impl AsRef<std::path::Path>,
        key: Option<dolang_rpc::auth::AuthKey>,
    ) -> error::Result<Self> {
        client::Client::connect_with_key(path, key)
            .await
            .map(Self::from_client)
    }

    /// Connects using an existing Unix-domain stream.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub async fn from_stream(stream: tokio::net::UnixStream) -> error::Result<Self> {
        client::Client::from_stream(stream)
            .await
            .map(Self::from_client)
    }

    /// Starts a VFS on an already-connected Unix-domain socket file
    /// descriptor.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub async fn from_owned_fd(value: std::os::fd::OwnedFd) -> error::Result<Self> {
        client::Client::from_owned_fd(value)
            .await
            .map(Self::from_client)
    }

    /// Starts a VFS on an already-connected Unix-domain socket file
    /// descriptor, proving knowledge of a pre-shared key.
    #[cfg(unix)]
    pub async fn from_owned_fd_with_key(
        value: std::os::fd::OwnedFd,
        key: Option<dolang_rpc::auth::AuthKey>,
    ) -> error::Result<Self> {
        client::Client::from_owned_fd_with_key(value, key)
            .await
            .map(Self::from_client)
    }

    /// Starts a VFS on the server end of a connected Windows named pipe.
    ///
    /// # Safety
    ///
    /// `server_process` must identify the trusted process at the other end of
    /// the pipe. That process can transfer handles which this process adopts.
    #[cfg(windows)]
    pub async unsafe fn from_named_pipe_server(
        pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        server_process: std::os::windows::io::OwnedHandle,
    ) -> error::Result<Self> {
        unsafe { client::Client::from_named_pipe_server(pipe, server_process) }
            .await
            .map(Self::from_client)
    }

    /// Returns whether this VFS accesses the local process directly.
    pub fn is_direct(&self) -> bool {
        matches!(&self.inner, VfsInner::Direct(_))
    }

    /// Stops a remote backend. Direct backends require no shutdown.
    pub async fn stop(&self) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.stop().await,
            VfsInner::Direct(_) => Ok(()),
        }
    }

    /// Closes a remote backend. Direct backends require no shutdown.
    pub async fn close(self) {
        match self.inner {
            VfsInner::Client(client) => client.close().await,
            VfsInner::Direct(_) => {}
        }
    }

    /// Calls a registered VFS extension, dispatching directly in-process or
    /// over RPC depending on which backend this `Vfs` wraps.
    pub async fn call_extension<T: VfsExtension>(
        &self,
        request: T::Request,
    ) -> error::Result<T::Response> {
        match &self.inner {
            VfsInner::Client(client) => client.call_extension::<T>(request).await,
            VfsInner::Direct(direct) => direct.call_extension::<T>(request).await,
        }
    }

    /// Iterates the target's initial process environment.
    pub fn env(&self) -> Box<dyn Iterator<Item = (String, String)> + '_> {
        match &self.inner {
            VfsInner::Client(client) => client.env(),
            VfsInner::Direct(direct) => direct.env(),
        }
    }

    /// Returns the target's initial working directory.
    pub fn cwd(&self) -> typed_path::Utf8TypedPath<'_> {
        match &self.inner {
            VfsInner::Client(vfs) => vfs.cwd(),
            VfsInner::Direct(vfs) => vfs.cwd(),
        }
    }

    /// Returns the target process executable.
    pub fn current_exe(&self) -> typed_path::Utf8TypedPath<'_> {
        match &self.inner {
            VfsInner::Client(vfs) => vfs.current_exe(),
            VfsInner::Direct(vfs) => vfs.current_exe(),
        }
    }

    /// Returns target platform information.
    pub fn target(&self) -> &target::TargetInfo {
        match &self.inner {
            VfsInner::Client(vfs) => vfs.target(),
            VfsInner::Direct(vfs) => vfs.target(),
        }
    }

    /// Returns the target's initial security context.
    pub fn security(&self) -> &security::SecurityInfo {
        match &self.inner {
            VfsInner::Client(vfs) => vfs.security(),
            VfsInner::Direct(vfs) => vfs.security(),
        }
    }

    /// Returns supported VFS extension protocol versions.
    pub fn extensions(&self) -> &extension::ExtensionSet {
        match &self.inner {
            VfsInner::Client(vfs) => vfs.extensions(),
            VfsInner::Direct(vfs) => vfs.extensions(),
        }
    }

    /// Creates a file-open options builder.
    pub fn open_options(&self) -> file::OpenOptions<'_> {
        match &self.inner {
            VfsInner::Client(client) => file::OpenOptions::client(client.open_options()),
            VfsInner::Direct(direct) => file::OpenOptions::direct(direct.open_options()),
        }
    }

    /// Creates a command builder for `program`.
    pub fn command(&self, program: typed_path::Utf8TypedPath<'_>) -> process::Command<'_> {
        process::Command::new(self, program)
    }

    /// Connects to a VFS agent over a Unix-domain socket.
    ///
    /// `key` is an optional pre-shared key that both ends must prove knowledge
    /// of during negotiation. It is what identifies the intended agent when
    /// the socket's permissions cannot; the concrete client accepts the same
    /// key when connecting.
    pub async fn unix_socket(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        key: Option<&[u8]>,
    ) -> error::Result<Vfs> {
        match &self.inner {
            VfsInner::Client(client) => client.unix_socket(path, key).await,
            VfsInner::Direct(direct) => direct.unix_socket(path, key).await,
        }
    }

    /// Starts a Windows administrative VFS session.
    pub async fn windows_admin(
        &self,
        cwd: typed_path::Utf8TypedPath<'_>,
        env: HashMap<String, Option<String>>,
        elevate: bool,
    ) -> error::Result<Vfs> {
        match &self.inner {
            VfsInner::Client(client) => client.windows_admin(cwd, env, elevate).await,
            VfsInner::Direct(direct) => direct.windows_admin(cwd, env, elevate).await,
        }
    }

    /// Creates a connected writable and readable pipe endpoint.
    ///
    /// `buf_size` is a best-effort kernel buffer size hint. Backends that
    /// cannot honor the hint use their default buffer size.
    pub async fn pipe(
        &self,
        buf_size: Option<usize>,
    ) -> error::Result<(process::StdioSend, process::StdioRecv)> {
        match &self.inner {
            VfsInner::Client(client) => client.pipe(buf_size).await,
            VfsInner::Direct(direct) => direct.pipe(buf_size).await,
        }
    }

    /// Resolves a Unix user ID to a name.
    pub async fn user_name(&self, uid: u32) -> error::Result<String> {
        match &self.inner {
            VfsInner::Client(client) => client.user_name(uid).await,
            VfsInner::Direct(direct) => direct.user_name(uid).await,
        }
    }

    /// Resolves a Unix user name to an ID.
    pub async fn user_id(&self, name: &str) -> error::Result<u32> {
        match &self.inner {
            VfsInner::Client(client) => client.user_id(name).await,
            VfsInner::Direct(direct) => direct.user_id(name).await,
        }
    }

    /// Resolves a Unix group ID to a name.
    pub async fn group_name(&self, gid: u32) -> error::Result<String> {
        match &self.inner {
            VfsInner::Client(client) => client.group_name(gid).await,
            VfsInner::Direct(direct) => direct.group_name(gid).await,
        }
    }

    /// Resolves a Unix group name to an ID.
    pub async fn group_id(&self, name: &str) -> error::Result<u32> {
        match &self.inner {
            VfsInner::Client(client) => client.group_id(name).await,
            VfsInner::Direct(direct) => direct.group_id(name).await,
        }
    }

    /// Resolves a Windows SID to its account name.
    pub async fn sid_name(&self, sid: &Sid) -> error::Result<security::SidName> {
        match &self.inner {
            VfsInner::Client(client) => client.sid_name(sid).await,
            VfsInner::Direct(direct) => direct.sid_name(sid).await,
        }
    }

    /// Resolves a Windows account name to its SID.
    pub async fn account_name(&self, name: &str) -> error::Result<security::SidName> {
        match &self.inner {
            VfsInner::Client(client) => client.account_name(name).await,
            VfsInner::Direct(direct) => direct.account_name(name).await,
        }
    }

    /// Converts a principal ID from one representation to another (e.g. a
    /// Unix uid/gid to/from a macOS principal UUID).
    pub async fn resolve_principal_id(
        &self,
        input: security::PrincipalId,
        want: security::PrincipalIdKind,
    ) -> error::Result<security::PrincipalId> {
        match &self.inner {
            VfsInner::Client(client) => client.resolve_principal_id(input, want).await,
            VfsInner::Direct(direct) => direct.resolve_principal_id(input, want).await,
        }
    }

    /// Opens a directory iterator.
    pub async fn read_dir(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<directory::ReadDir> {
        match &self.inner {
            VfsInner::Client(client) => client.read_dir(path).await,
            VfsInner::Direct(direct) => direct.read_dir(path).await,
        }
    }

    /// Finds an executable using a target search path.
    pub async fn which(
        &self,
        program: typed_path::Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<typed_path::Utf8TypedPath<'_>>,
    ) -> error::Result<Option<typed_path::Utf8TypedPathBuf>> {
        match &self.inner {
            VfsInner::Client(client) => client.which(program, path, cwd).await,
            VfsInner::Direct(direct) => direct.which(program, path, cwd).await,
        }
    }

    /// Resolves a target-specific well-known path.
    pub async fn well_known_path(
        &self,
        key: path::WellKnownPath,
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> error::Result<typed_path::Utf8TypedPathBuf> {
        match &self.inner {
            VfsInner::Client(client) => client.well_known_path(key, app, env).await,
            VfsInner::Direct(direct) => direct.well_known_path(key, app, env).await,
        }
    }

    /// Clears target-side cached state.
    pub async fn clear_cache(&self) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.clear_cache().await,
            VfsInner::Direct(direct) => direct.clear_cache().await,
        }
    }

    /// Lists extended attributes for a path.
    pub async fn xattrs(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        namespace: file::XattrNamespace<'_>,
        follow: bool,
    ) -> error::Result<Vec<file::XattrEntry>> {
        match &self.inner {
            VfsInner::Client(client) => client.xattrs(path, namespace, follow).await,
            VfsInner::Direct(direct) => direct.xattrs(path, namespace, follow).await,
        }
    }

    /// Lists alternate data streams for a path.
    pub async fn streams(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        follow: bool,
    ) -> error::Result<Vec<file::StreamEntry>> {
        match &self.inner {
            VfsInner::Client(client) => client.streams(path, follow).await,
            VfsInner::Direct(direct) => direct.streams(path, follow).await,
        }
    }

    /// Reads an extended attribute for a path.
    pub async fn xattr(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> error::Result<Vec<u8>> {
        match &self.inner {
            VfsInner::Client(client) => client.xattr(path, name, namespace, follow).await,
            VfsInner::Direct(direct) => direct.xattr(path, name, namespace, follow).await,
        }
    }

    /// Creates or replaces an extended attribute for a path.
    pub async fn set_xattr(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => {
                client.set_xattr(path, name, namespace, value, follow).await
            }
            VfsInner::Direct(direct) => {
                direct.set_xattr(path, name, namespace, value, follow).await
            }
        }
    }

    /// Removes an extended attribute from a path.
    pub async fn remove_xattr(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.remove_xattr(path, name, namespace, follow).await,
            VfsInner::Direct(direct) => direct.remove_xattr(path, name, namespace, follow).await,
        }
    }

    /// Removes a file or symlink.
    pub async fn remove(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.remove(path, all, ignore).await,
            VfsInner::Direct(direct) => direct.remove(path, all, ignore).await,
        }
    }

    /// Returns metadata without following the final symlink.
    pub async fn metadata(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<metadata::Metadata> {
        match &self.inner {
            VfsInner::Client(client) => client.metadata(path).await,
            VfsInner::Direct(direct) => direct.metadata(path).await,
        }
    }

    /// Returns filesystem metadata for a path.
    pub async fn fs_metadata(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        follow: bool,
    ) -> error::Result<metadata::FsMetadata> {
        match &self.inner {
            VfsInner::Client(client) => client.fs_metadata(path, follow).await,
            VfsInner::Direct(direct) => direct.fs_metadata(path, follow).await,
        }
    }

    /// Returns the ACL of the requested `kind` for a path. See
    /// [`file::File::acl`] for `default`'s meaning.
    pub async fn acl(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        kind: security::AclKind,
        default: bool,
        follow: bool,
    ) -> error::Result<Option<security::Acl>> {
        match &self.inner {
            VfsInner::Client(client) => client.acl(path, kind, default, follow).await,
            VfsInner::Direct(direct) => direct.acl(path, kind, default, follow).await,
        }
    }

    /// Sets or removes the ACL for a path. See [`file::File::set_acl`] for
    /// `default`'s meaning.
    pub async fn set_acl(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        kind: security::AclKind,
        acl: Option<&security::Acl>,
        default: bool,
        follow: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.set_acl(path, kind, acl, default, follow).await,
            VfsInner::Direct(direct) => direct.set_acl(path, kind, acl, default, follow).await,
        }
    }

    /// Returns the Windows security descriptor for a path.
    pub async fn sec_desc(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        mask: dolang_winterop::security::SecInfo,
        follow: bool,
    ) -> error::Result<SecDesc> {
        match &self.inner {
            VfsInner::Client(client) => client.sec_desc(path, mask, follow).await,
            VfsInner::Direct(direct) => direct.sec_desc(path, mask, follow).await,
        }
    }

    /// Replaces the Windows security descriptor for a path.
    pub async fn set_sec_desc(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.set_sec_desc(path, sec_desc, follow).await,
            VfsInner::Direct(direct) => direct.set_sec_desc(path, sec_desc, follow).await,
        }
    }

    /// Creates a directory, optionally including missing parents.
    pub async fn create_dir(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        all: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.create_dir(path, all).await,
            VfsInner::Direct(direct) => direct.create_dir(path, all).await,
        }
    }

    /// Removes a directory.
    pub async fn remove_dir(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.remove_dir(path, all, ignore).await,
            VfsInner::Direct(direct) => direct.remove_dir(path, all, ignore).await,
        }
    }

    /// Copies a path, optionally including directory contents.
    pub async fn copy(
        &self,
        from: typed_path::Utf8TypedPath<'_>,
        to: typed_path::Utf8TypedPath<'_>,
        all: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.copy(from, to, all).await,
            VfsInner::Direct(direct) => direct.copy(from, to, all).await,
        }
    }

    /// Renames a path.
    pub async fn rename(
        &self,
        from: typed_path::Utf8TypedPath<'_>,
        to: typed_path::Utf8TypedPath<'_>,
        replace: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.rename(from, to, replace).await,
            VfsInner::Direct(direct) => direct.rename(from, to, replace).await,
        }
    }

    /// Moves a path, optionally including directory contents.
    pub async fn move_(
        &self,
        from: typed_path::Utf8TypedPath<'_>,
        to: typed_path::Utf8TypedPath<'_>,
        all: bool,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.move_(from, to, all).await,
            VfsInner::Direct(direct) => direct.move_(from, to, all).await,
        }
    }

    /// Creates a symbolic link using `cwd` to interpret relative source paths.
    pub async fn symlink(
        &self,
        cwd: typed_path::Utf8TypedPath<'_>,
        src: typed_path::Utf8TypedPath<'_>,
        dst: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.symlink(cwd, src, dst).await,
            VfsInner::Direct(direct) => direct.symlink(cwd, src, dst).await,
        }
    }

    /// Creates a hard link.
    pub async fn hard_link(
        &self,
        src: typed_path::Utf8TypedPath<'_>,
        dst: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.hard_link(src, dst).await,
            VfsInner::Direct(direct) => direct.hard_link(src, dst).await,
        }
    }

    /// Creates a symbolic link to a directory.
    pub async fn symlink_dir(
        &self,
        src: typed_path::Utf8TypedPath<'_>,
        dst: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.symlink_dir(src, dst).await,
            VfsInner::Direct(direct) => direct.symlink_dir(src, dst).await,
        }
    }

    /// Creates a symbolic link to a file.
    pub async fn symlink_file(
        &self,
        src: typed_path::Utf8TypedPath<'_>,
        dst: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.symlink_file(src, dst).await,
            VfsInner::Direct(direct) => direct.symlink_file(src, dst).await,
        }
    }

    /// Returns metadata without following the final symlink.
    pub async fn symlink_metadata(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<metadata::Metadata> {
        match &self.inner {
            VfsInner::Client(client) => client.symlink_metadata(path).await,
            VfsInner::Direct(direct) => direct.symlink_metadata(path).await,
        }
    }

    /// Applies a metadata patch to every path.
    pub async fn set_metadata(
        &self,
        paths: &[typed_path::Utf8TypedPathBuf],
        patch: metadata::MetadataPatch,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.set_metadata(paths, patch).await,
            VfsInner::Direct(direct) => direct.set_metadata(paths, patch).await,
        }
    }

    /// Resolves a path to its canonical absolute form.
    pub async fn canonicalize(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<typed_path::Utf8TypedPathBuf> {
        match &self.inner {
            VfsInner::Client(client) => client.canonicalize(path).await,
            VfsInner::Direct(direct) => direct.canonicalize(path).await,
        }
    }

    /// Returns the destination of a symbolic link.
    pub async fn read_link(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
    ) -> error::Result<typed_path::Utf8TypedPathBuf> {
        match &self.inner {
            VfsInner::Client(client) => client.read_link(path).await,
            VfsInner::Direct(direct) => direct.read_link(path).await,
        }
    }

    /// Checks whether the process can access a path with the requested permissions.
    pub async fn access(
        &self,
        path: typed_path::Utf8TypedPath<'_>,
        mode: file::AccessFlags,
    ) -> error::Result<()> {
        match &self.inner {
            VfsInner::Client(client) => client.access(path, mode).await,
            VfsInner::Direct(direct) => direct.access(path, mode).await,
        }
    }

    /// Expands a glob pattern beneath `root`.
    pub async fn glob(
        &self,
        pattern: impl Into<String>,
        root: typed_path::Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> error::Result<Vec<typed_path::Utf8TypedPathBuf>> {
        let pattern = pattern.into();

        match &self.inner {
            VfsInner::Client(client) => {
                client.glob(pattern, root, follow_symlinks, max_depth).await
            }
            VfsInner::Direct(direct) => {
                direct.glob(pattern, root, follow_symlinks, max_depth).await
            }
        }
    }
}
