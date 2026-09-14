use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    io::IsTerminal,
    pin::Pin,
    rc::Rc,
};

use dolang::runtime::{
    Sym, Type,
    object::{FlagLikeExt, Flags},
    strand::LocalKey,
    value::TypeObject,
    vm::{Builder, Register, Stateful},
};
use tokio::{
    io::{self as tio, AsyncWrite, stderr},
    sync::Mutex,
};

use crate::{
    console::HostConsole,
    error::{
        AddrInUseError, AddrNotAvailableError, AlreadyExistsError, ArgumentListTooLongError,
        BrokenPipeError, ConnectionAbortedError, ConnectionRefusedError, ConnectionResetError,
        CrossesDevicesError, DeadlockError, DirectoryNotEmptyError, ExecutableFileBusyError,
        FileTooLargeError, HostUnreachableError, InterruptedError, InvalidDataError,
        InvalidFilenameError, InvalidInputError, IsADirectoryError, NetworkDownError,
        NetworkUnreachableError, NotADirectoryError, NotConnectedError, NotFoundError,
        NotSeekableError, OutOfMemoryError, PermissionDeniedError, ProcError, QuotaExceededError,
        ReadOnlyFilesystemError, ResourceBusyError, StaleNetworkFileHandleError, StorageFullError,
        SysError, SysErrorObject, TimedOutError, TooManyLinksError, UnexpectedEofError,
        UnsupportedError, WouldBlockError, WriteZeroError,
    },
    error_code::{CodeObject, Errno, ErrorCode, FreeBsdErrno, LinuxErrno, MacosErrno, WinError},
    fs::{
        file::File,
        file_lock::FileLock,
        fs_metadata::FsMetadata,
        metadata::{Metadata, Mode},
        path::{Path, UnixPath, WindowsPath},
        readdir::{DirEntry, DirEntryIter},
        stream::{StreamEntry, StreamIter},
        xattr::{XattrEntry, XattrIter},
    },
    geometry::HostGeometry,
    local::Local,
    pipe_channel::{PipeReceiver, PipeSender},
    proc::{Capture, Info as ProcInfo, Proc, Procs, Status as ProcStatus},
    program::Program,
    security::{
        AccessMask, Ace, AceFlags, Acl, Identity, MacosAceFlags, MacosAceMask, MacosAceObject,
        MacosAclObject, Nfs4AceFlags, Nfs4AceMask, Nfs4AceObject, Nfs4AclObject, Permission,
        PosixAceObject, PosixAclObject, SecDesc, SecDescControl, SecInfo, Sid, SidName, TokenGroup,
        TokenGroupAttributes, TokenInfo, WellKnownSids,
    },
    shell::{Stderr, Stdin, Stdout, Vfs},
    shell_args::ArgsData,
    sys::{CpuInfo, OsInfo},
};

#[derive(Clone)]
pub enum ProgramSource {
    Path(std::path::PathBuf),
    Module(String),
}

/// State registered eagerly: the host console, the process's standard streams,
/// and the strand-local key every other part of the extension reads.
///
/// Everything else lives in the lazy setup states below, one per group of
/// modules. Each copies `local` so it can reach the strand's context without
/// looking this state up again.
pub(crate) struct Global<'v> {
    pub(crate) terminal: Terminal,
    pub(crate) stdio: Stdio,
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) args: RefCell<ArgsData>,
    pub(crate) program: RefCell<Option<ProgramSource>>,
}

pub(crate) struct Types<'v> {
    pub(crate) host_console: Type<'v, HostConsole>,
    pub(crate) host_geometry: Type<'v, HostGeometry>,
}

pub(crate) struct Syms<'v> {
    pub(crate) chunk: Sym<'v, 'v>,
    pub(crate) line: Sym<'v, 'v>,
}

/// The process's standard streams.
///
/// These live here rather than inside the `shell.stdin`/`stdout`/`stderr`
/// handle objects, which are stateless. Two consequences, both load-bearing:
///
/// - There is exactly one `BufReader` over stdin. A second one would silently
///   split buffered input, so reading through `shell.stdin` and through the
///   implicit input stay coherent no matter how many handle objects exist.
/// - Writes serialize on a mutex rather than on a per-object GC borrow, so
///   concurrent writes from forked strands queue instead of failing with a
///   concurrency error.
///
/// It also means handle instances are interchangeable, so nothing needs to root
/// a particular one.
pub(crate) struct Stdio {
    pub(crate) stdin: Mutex<tio::BufReader<tio::Stdin>>,
    pub(crate) stdout: Mutex<tio::Stdout>,
    pub(crate) stderr: Mutex<tio::Stderr>,
}

pub(crate) struct Terminal {
    /// The writer, behind an async mutex so it can be held across await
    /// points by concurrent strands without conflict.
    pub(crate) writer: Mutex<Pin<Box<dyn AsyncWrite>>>,
    pub(crate) redirected: Cell<bool>,
    /// Whether stdout was a terminal at startup (cached to avoid repeated
    /// syscalls).
    pub(crate) stdout_is_terminal: bool,
    /// Whether stderr is a terminal, for every purpose that answer feeds:
    /// `HostConsole::is_tty`, the tty-detection fallback of [`Self::ansi`], and
    /// [`crate::stderr_is_tty`]. Cached at startup — real terminal-ness
    /// cannot change mid-process — and already folds in `DOLANG_CONSOLE`'s
    /// `tty=` override, so every reader downstream gets the overridden
    /// answer for free rather than each needing to know the override exists.
    pub(crate) stderr_is_terminal: bool,
    /// Whether ANSI styling should be emitted to stderr.
    pub(crate) ansi: bool,
    /// Parsed `DOLANG_CONSOLE`, consulted directly only by `geometry()`
    /// (`rows`/`cols` have no other home to fold into).
    pub(crate) console_override: ConsoleOverride,
}

fn ansi_enabled(
    stderr_is_terminal: bool,
    force_color: Option<&OsStr>,
    no_color: Option<&OsStr>,
) -> bool {
    if let Some(force_color) = force_color {
        force_color != "0"
    } else if no_color.is_some_and(|no_color| !no_color.is_empty()) {
        false
    } else {
        stderr_is_terminal
    }
}

/// Explicit console overrides from `DOLANG_CONSOLE`, e.g.
/// `tty=false,cols=120,style=true`.
///
/// A comma-separated list of `key=value` pairs. Each key is independent and
/// optional; an unset key falls through to normal detection. Unknown keys and
/// unparseable values are ignored rather than erroring — a malformed
/// environment variable must not be able to crash startup, the same
/// forgiving posture `FORCE_COLOR`/`NO_COLOR` already have (no value of
/// either is rejected).
///
/// This exists for tests and CI that need deterministic console behavior
/// regardless of the real stderr: forcing `tty=false` for reproducible plain
/// output, or `tty=true` with explicit `rows`/`cols` to exercise
/// terminal-shaped rendering (styling, `progress`) through a capture that
/// isn't a real terminal.
#[derive(Default)]
pub(crate) struct ConsoleOverride {
    pub(crate) tty: Option<bool>,
    pub(crate) rows: Option<u16>,
    pub(crate) cols: Option<u16>,
    pub(crate) style: Option<bool>,
}

fn parse_override_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

impl ConsoleOverride {
    fn parse(input: Option<&str>) -> Self {
        let mut result = Self::default();
        let Some(input) = input else {
            return result;
        };
        for entry in input.split(',') {
            let Some((key, value)) = entry.split_once('=') else {
                continue;
            };
            match key.trim() {
                "tty" => result.tty = parse_override_bool(value.trim()),
                "style" => result.style = parse_override_bool(value.trim()),
                "rows" => result.rows = value.trim().parse().ok(),
                "cols" => result.cols = value.trim().parse().ok(),
                _ => {}
            }
        }
        result
    }
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let console = dolang_ext_term::console_type(builder);
        let host_console = builder
            .build_type::<HostConsole>((), ())
            .nominal_supertype(console)
            .build();

        let geometry = dolang_ext_term::geometry_type(builder);
        let host_geometry = builder
            .build_type::<HostGeometry>((), ())
            .nominal_supertype(geometry)
            .build();

        let console_override =
            ConsoleOverride::parse(std::env::var("DOLANG_CONSOLE").ok().as_deref());
        let stderr_is_terminal = console_override
            .tty
            .unwrap_or_else(|| std::io::stderr().is_terminal());
        let ansi = match console_override.style {
            Some(style) => style,
            None => ansi_enabled(
                stderr_is_terminal,
                std::env::var_os("FORCE_COLOR").as_deref(),
                std::env::var_os("NO_COLOR").as_deref(),
            ),
        };
        Self {
            stdio: Stdio {
                stdin: Mutex::new(tio::BufReader::new(tio::stdin())),
                stdout: Mutex::new(tio::stdout()),
                stderr: Mutex::new(tio::stderr()),
            },
            terminal: Terminal {
                writer: Mutex::new(Box::pin(stderr())),
                redirected: Cell::new(false),
                stdout_is_terminal: std::io::stdout().is_terminal(),
                stderr_is_terminal,
                ansi,
                console_override,
            },
            types: Types {
                host_console,
                host_geometry,
            },
            syms: Syms {
                chunk: builder.sym("CHUNK"),
                line: builder.sym("LINE"),
            },
            local: builder.local(),
            args: RefCell::new(Rc::from([])),
            program: RefCell::new(None),
        }
    }
}

/// State for the system error types and the `sys.<platform>` modules.
pub(crate) struct ErrorGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: ErrorTypes<'v>,
    pub(crate) syms: ErrorSyms<'v>,
}

pub(crate) struct ErrorTypes<'v> {
    pub(crate) error_code: Type<'v, CodeObject<ErrorCode>>,
    pub(crate) errno: Type<'v, CodeObject<Errno>>,
    pub(crate) freebsd_errno: Type<'v, CodeObject<FreeBsdErrno>>,
    pub(crate) linux_errno: Type<'v, CodeObject<LinuxErrno>>,
    pub(crate) macos_errno: Type<'v, CodeObject<MacosErrno>>,
    pub(crate) win_error: Type<'v, CodeObject<WinError>>,
    pub(crate) sys_error: Type<'v, SysErrorObject<SysError>>,
    pub(crate) invalid_input: Type<'v, SysErrorObject<InvalidInputError>>,
    pub(crate) not_found: Type<'v, SysErrorObject<NotFoundError>>,
    pub(crate) permission_denied: Type<'v, SysErrorObject<PermissionDeniedError>>,
    pub(crate) already_exists: Type<'v, SysErrorObject<AlreadyExistsError>>,
    pub(crate) timed_out: Type<'v, SysErrorObject<TimedOutError>>,
    pub(crate) unsupported: Type<'v, SysErrorObject<UnsupportedError>>,
    pub(crate) connection_refused: Type<'v, SysErrorObject<ConnectionRefusedError>>,
    pub(crate) connection_reset: Type<'v, SysErrorObject<ConnectionResetError>>,
    pub(crate) host_unreachable: Type<'v, SysErrorObject<HostUnreachableError>>,
    pub(crate) network_unreachable: Type<'v, SysErrorObject<NetworkUnreachableError>>,
    pub(crate) connection_aborted: Type<'v, SysErrorObject<ConnectionAbortedError>>,
    pub(crate) not_connected: Type<'v, SysErrorObject<NotConnectedError>>,
    pub(crate) addr_in_use: Type<'v, SysErrorObject<AddrInUseError>>,
    pub(crate) addr_not_available: Type<'v, SysErrorObject<AddrNotAvailableError>>,
    pub(crate) network_down: Type<'v, SysErrorObject<NetworkDownError>>,
    pub(crate) broken_pipe: Type<'v, SysErrorObject<BrokenPipeError>>,
    pub(crate) would_block: Type<'v, SysErrorObject<WouldBlockError>>,
    pub(crate) not_adirectory: Type<'v, SysErrorObject<NotADirectoryError>>,
    pub(crate) is_adirectory: Type<'v, SysErrorObject<IsADirectoryError>>,
    pub(crate) directory_not_empty: Type<'v, SysErrorObject<DirectoryNotEmptyError>>,
    pub(crate) read_only_filesystem: Type<'v, SysErrorObject<ReadOnlyFilesystemError>>,
    pub(crate) stale_network_file_handle: Type<'v, SysErrorObject<StaleNetworkFileHandleError>>,
    pub(crate) write_zero: Type<'v, SysErrorObject<WriteZeroError>>,
    pub(crate) storage_full: Type<'v, SysErrorObject<StorageFullError>>,
    pub(crate) not_seekable: Type<'v, SysErrorObject<NotSeekableError>>,
    pub(crate) quota_exceeded: Type<'v, SysErrorObject<QuotaExceededError>>,
    pub(crate) file_too_large: Type<'v, SysErrorObject<FileTooLargeError>>,
    pub(crate) resource_busy: Type<'v, SysErrorObject<ResourceBusyError>>,
    pub(crate) executable_file_busy: Type<'v, SysErrorObject<ExecutableFileBusyError>>,
    pub(crate) deadlock: Type<'v, SysErrorObject<DeadlockError>>,
    pub(crate) crosses_devices: Type<'v, SysErrorObject<CrossesDevicesError>>,
    pub(crate) too_many_links: Type<'v, SysErrorObject<TooManyLinksError>>,
    pub(crate) invalid_filename: Type<'v, SysErrorObject<InvalidFilenameError>>,
    pub(crate) argument_list_too_long: Type<'v, SysErrorObject<ArgumentListTooLongError>>,
    pub(crate) invalid_data: Type<'v, SysErrorObject<InvalidDataError>>,
    pub(crate) interrupted: Type<'v, SysErrorObject<InterruptedError>>,
    pub(crate) unexpected_eof: Type<'v, SysErrorObject<UnexpectedEofError>>,
    pub(crate) out_of_memory: Type<'v, SysErrorObject<OutOfMemoryError>>,
    pub(crate) proc_error: Type<'v, ProcError>,
}

pub(crate) struct ErrorSyms<'v> {
    pub(crate) code: Sym<'v, 'v>,
}

pub struct ErrorTag;

impl<'v> Stateful<'v> for ErrorGlobal<'v> {
    type Tag = ErrorTag;
}

impl<'v> ErrorGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        let error_code = builder.register_type::<CodeObject<ErrorCode>>();
        let errno = builder
            .build_type::<CodeObject<Errno>>((), ())
            .nominal_supertype(error_code)
            .build();
        let sys_error = builder
            .build_type::<SysErrorObject<SysError>>((), ())
            .nominal_supertype(TypeObject::RuntimeError)
            .build();
        Self {
            local,
            types: ErrorTypes {
                error_code,
                errno,
                freebsd_errno: builder
                    .build_type::<CodeObject<FreeBsdErrno>>((), ())
                    .nominal_supertype(errno)
                    .build(),
                linux_errno: builder
                    .build_type::<CodeObject<LinuxErrno>>((), ())
                    .nominal_supertype(errno)
                    .build(),
                macos_errno: builder
                    .build_type::<CodeObject<MacosErrno>>((), ())
                    .nominal_supertype(errno)
                    .build(),
                win_error: builder
                    .build_type::<CodeObject<WinError>>((), ())
                    .nominal_supertype(error_code)
                    .build(),
                sys_error,
                invalid_input: builder
                    .build_type::<SysErrorObject<InvalidInputError>>((), ())
                    .nominal_supertype(sys_error)
                    .nominal_supertype(TypeObject::ValueError)
                    .build(),
                not_found: builder
                    .build_type::<SysErrorObject<NotFoundError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                permission_denied: builder
                    .build_type::<SysErrorObject<PermissionDeniedError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                already_exists: builder
                    .build_type::<SysErrorObject<AlreadyExistsError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                timed_out: builder
                    .build_type::<SysErrorObject<TimedOutError>>((), ())
                    .nominal_supertype(sys_error)
                    .nominal_supertype(TypeObject::TimedOutError)
                    .build(),
                unsupported: builder
                    .build_type::<SysErrorObject<UnsupportedError>>((), ())
                    .nominal_supertype(sys_error)
                    .nominal_supertype(TypeObject::UnsupportedError)
                    .build(),
                connection_refused: builder
                    .build_type::<SysErrorObject<ConnectionRefusedError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                connection_reset: builder
                    .build_type::<SysErrorObject<ConnectionResetError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                host_unreachable: builder
                    .build_type::<SysErrorObject<HostUnreachableError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                network_unreachable: builder
                    .build_type::<SysErrorObject<NetworkUnreachableError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                connection_aborted: builder
                    .build_type::<SysErrorObject<ConnectionAbortedError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                not_connected: builder
                    .build_type::<SysErrorObject<NotConnectedError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                addr_in_use: builder
                    .build_type::<SysErrorObject<AddrInUseError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                addr_not_available: builder
                    .build_type::<SysErrorObject<AddrNotAvailableError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                network_down: builder
                    .build_type::<SysErrorObject<NetworkDownError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                broken_pipe: builder
                    .build_type::<SysErrorObject<BrokenPipeError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                would_block: builder
                    .build_type::<SysErrorObject<WouldBlockError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                not_adirectory: builder
                    .build_type::<SysErrorObject<NotADirectoryError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                is_adirectory: builder
                    .build_type::<SysErrorObject<IsADirectoryError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                directory_not_empty: builder
                    .build_type::<SysErrorObject<DirectoryNotEmptyError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                read_only_filesystem: builder
                    .build_type::<SysErrorObject<ReadOnlyFilesystemError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                stale_network_file_handle: builder
                    .build_type::<SysErrorObject<StaleNetworkFileHandleError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                write_zero: builder
                    .build_type::<SysErrorObject<WriteZeroError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                storage_full: builder
                    .build_type::<SysErrorObject<StorageFullError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                not_seekable: builder
                    .build_type::<SysErrorObject<NotSeekableError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                quota_exceeded: builder
                    .build_type::<SysErrorObject<QuotaExceededError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                file_too_large: builder
                    .build_type::<SysErrorObject<FileTooLargeError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                resource_busy: builder
                    .build_type::<SysErrorObject<ResourceBusyError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                executable_file_busy: builder
                    .build_type::<SysErrorObject<ExecutableFileBusyError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                deadlock: builder
                    .build_type::<SysErrorObject<DeadlockError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                crosses_devices: builder
                    .build_type::<SysErrorObject<CrossesDevicesError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                too_many_links: builder
                    .build_type::<SysErrorObject<TooManyLinksError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                invalid_filename: builder
                    .build_type::<SysErrorObject<InvalidFilenameError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                argument_list_too_long: builder
                    .build_type::<SysErrorObject<ArgumentListTooLongError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                invalid_data: builder
                    .build_type::<SysErrorObject<InvalidDataError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                interrupted: builder
                    .build_type::<SysErrorObject<InterruptedError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                unexpected_eof: builder
                    .build_type::<SysErrorObject<UnexpectedEofError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                out_of_memory: builder
                    .build_type::<SysErrorObject<OutOfMemoryError>>((), ())
                    .nominal_supertype(sys_error)
                    .build(),
                proc_error: builder.register_type(),
            },
            syms: ErrorSyms {
                code: builder.sym("code"),
            },
        }
    }
}

/// State for the `fs` modules, including the path types.
pub(crate) struct FsGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: FsTypes<'v>,
    pub(crate) syms: FsSyms<'v>,
}

pub(crate) struct FsTypes<'v> {
    pub(crate) path: Type<'v, Path>,
    pub(crate) unix_path: Type<'v, UnixPath>,
    pub(crate) windows_path: Type<'v, WindowsPath>,
    pub(crate) xattr_entry: Type<'v, XattrEntry>,
    pub(crate) xattr_iter: Type<'v, XattrIter>,
    pub(crate) stream_entry: Type<'v, StreamEntry>,
    pub(crate) stream_iter: Type<'v, StreamIter>,
    pub(crate) metadata: Type<'v, Metadata>,
    pub(crate) fs_metadata: Type<'v, FsMetadata>,
    pub(crate) file: Type<'v, File<'v>>,
    pub(crate) file_lock: Type<'v, FileLock>,
    pub(crate) dir_entry: Type<'v, DirEntry>,
    pub(crate) dir_entry_iter: Type<'v, DirEntryIter>,
    pub(crate) glob_iter: Type<'v, crate::fs::glob::GlobIter>,
    pub(crate) mode: Type<'v, Flags<Mode>>,
}

pub(crate) struct FsSyms<'v> {
    pub(crate) any: Sym<'v, 'v>,
    pub(crate) auto: Sym<'v, 'v>,
    pub(crate) block_device: Sym<'v, 'v>,
    pub(crate) char_device: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) dir: Sym<'v, 'v>,
    pub(crate) fifo: Sym<'v, 'v>,
    pub(crate) file: Sym<'v, 'v>,
    pub(crate) link: Sym<'v, 'v>,
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) namespace_system: Sym<'v, 'v>,
    pub(crate) namespace_user: Sym<'v, 'v>,
    pub(crate) never: Sym<'v, 'v>,
    pub(crate) require: Sym<'v, 'v>,
    pub(crate) socket: Sym<'v, 'v>,
    pub(crate) symlink: Sym<'v, 'v>,
    pub(crate) target: Sym<'v, 'v>,
    pub(crate) unknown: Sym<'v, 'v>,
}

pub struct FsTag;

impl<'v> Stateful<'v> for FsGlobal<'v> {
    type Tag = FsTag;
}

impl<'v> FsGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        let path = builder.register_type::<Path>();
        let unix_path = builder
            .build_type::<UnixPath>((), ())
            .nominal_supertype(path)
            .build();
        let windows_path = builder
            .build_type::<WindowsPath>((), ())
            .nominal_supertype(path)
            .build();
        Self {
            local,
            types: FsTypes {
                path,
                unix_path,
                windows_path,
                xattr_entry: builder.register_type(),
                xattr_iter: builder.register_type(),
                stream_entry: builder.register_type(),
                stream_iter: builder.register_type(),
                metadata: builder.register_type(),
                fs_metadata: builder.register_type(),
                file: builder.register_type(),
                file_lock: builder.register_type(),
                dir_entry: builder.register_type(),
                dir_entry_iter: builder.register_type(),
                glob_iter: builder.register_type(),
                mode: Mode::register_type(builder),
            },
            syms: FsSyms {
                any: builder.sym("ANY"),
                auto: builder.sym("AUTO"),
                block_device: builder.sym("BLOCK_DEVICE"),
                char_device: builder.sym("CHAR_DEVICE"),
                close: builder.sym("close"),
                dir: builder.sym("DIR"),
                fifo: builder.sym("FIFO"),
                file: builder.sym("FILE"),
                link: builder.sym("LINK"),
                namespace: builder.sym("namespace"),
                namespace_system: builder.sym("SYSTEM"),
                namespace_user: builder.sym("USER"),
                never: builder.sym("NEVER"),
                require: builder.sym("REQUIRE"),
                socket: builder.sym("SOCKET"),
                symlink: builder.sym("SYMLINK"),
                target: builder.sym("TARGET"),
                unknown: builder.sym("UNKNOWN"),
            },
        }
    }
}

/// State for the `shell` module.
pub(crate) struct ShellGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: ShellTypes<'v>,
    pub(crate) syms: ShellSyms<'v>,
}

pub(crate) struct ShellTypes<'v> {
    pub(crate) stdin: Type<'v, Stdin>,
    pub(crate) stdout: Type<'v, Stdout>,
    pub(crate) stderr: Type<'v, Stderr>,
    pub(crate) vfs: Type<'v, Vfs>,
}

pub(crate) struct ShellSyms<'v> {
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) inherit: Sym<'v, 'v>,
    pub(crate) join: Sym<'v, 'v>,
}

pub struct ShellTag;

impl<'v> Stateful<'v> for ShellGlobal<'v> {
    type Tag = ShellTag;
}

impl<'v> ShellGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: ShellTypes {
                stdin: builder.register_type(),
                stdout: builder.register_type(),
                stderr: builder.register_type(),
                vfs: builder.register_type(),
            },
            syms: ShellSyms {
                close: builder.sym("close"),
                inherit: builder.sym("INHERIT"),
                join: builder.sym("join"),
            },
        }
    }
}

/// State for the `proc` modules.
pub(crate) struct ProcGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: ProcTypes<'v>,
    pub(crate) syms: ProcSyms<'v>,
}

pub(crate) struct ProcTypes<'v> {
    pub(crate) program: Type<'v, Program>,
    pub(crate) capture: Type<'v, Capture>,
    pub(crate) proc_info: Type<'v, ProcInfo>,
    pub(crate) procs: Type<'v, Procs>,
    pub(crate) proc_handle: Type<'v, Proc>,
    pub(crate) proc_status: Type<'v, ProcStatus>,
}

pub(crate) struct ProcSyms<'v> {
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) force: Sym<'v, 'v>,
    pub(crate) grace: Sym<'v, 'v>,
    pub(crate) mode: Sym<'v, 'v>,
    pub(crate) policy: Sym<'v, 'v>,
    pub(crate) signal: Sym<'v, 'v>,
    pub(crate) stderr: Sym<'v, 'v>,
    pub(crate) stdin: Sym<'v, 'v>,
    pub(crate) stdout: Sym<'v, 'v>,
    pub(crate) stdout_redirect: Sym<'v, 'v>,
}

pub struct ProcTag;

impl<'v> Stateful<'v> for ProcGlobal<'v> {
    type Tag = ProcTag;
}

impl<'v> ProcGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: ProcTypes {
                program: builder.register_type(),
                capture: builder.register_type(),
                proc_info: builder.register_type(),
                procs: builder.register_type(),
                proc_handle: builder.register_type(),
                proc_status: builder.register_type(),
            },
            syms: ProcSyms {
                close: builder.sym("close"),
                force: builder.sym("force"),
                grace: builder.sym("grace"),
                mode: builder.sym("mode"),
                policy: builder.sym("policy"),
                signal: builder.sym("signal"),
                stderr: builder.sym("stderr"),
                stdin: builder.sym("stdin"),
                stdout: builder.sym("stdout"),
                stdout_redirect: builder.sym("STDOUT"),
            },
        }
    }
}

/// State for pipe channels, which the runtime creates for pipelines and which
/// have no module of their own.
pub(crate) struct PipeGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: PipeTypes<'v>,
}

pub(crate) struct PipeTypes<'v> {
    pub(crate) pipe_receiver: Type<'v, PipeReceiver>,
    pub(crate) pipe_sender: Type<'v, PipeSender>,
}

pub struct PipeTag;

impl<'v> Stateful<'v> for PipeGlobal<'v> {
    type Tag = PipeTag;
}

impl<'v> PipeGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: PipeTypes {
                pipe_receiver: builder.register_type(),
                pipe_sender: builder.register_type(),
            },
        }
    }
}

/// State for the `sys` module.
pub(crate) struct SysGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: SysTypes<'v>,
}

pub(crate) struct SysTypes<'v> {
    pub(crate) os_info: Type<'v, OsInfo>,
    pub(crate) cpu_info: Type<'v, CpuInfo>,
}

pub struct SysTag;

impl<'v> Stateful<'v> for SysGlobal<'v> {
    type Tag = SysTag;
}

impl<'v> SysGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: SysTypes {
                os_info: builder.register_type(),
                cpu_info: builder.register_type(),
            },
        }
    }
}

/// Lazy setup tag for the `security` module, which has no state of its own.
pub struct SecurityTag;

/// State for the `security.unix` module.
pub(crate) struct UnixSecurityGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: UnixSecurityTypes<'v>,
    pub(crate) syms: UnixSecuritySyms<'v>,
}

pub(crate) struct UnixSecurityTypes<'v> {
    pub(crate) unix_identity: Type<'v, Identity>,
    pub(crate) posix_acl: Type<'v, PosixAclObject>,
    pub(crate) posix_ace: Type<'v, PosixAceObject>,
    pub(crate) permission: Type<'v, Flags<Permission>>,
}

pub(crate) struct UnixSecuritySyms<'v> {
    pub(crate) group: Sym<'v, 'v>,
    pub(crate) group_obj: Sym<'v, 'v>,
    pub(crate) mask: Sym<'v, 'v>,
    pub(crate) other: Sym<'v, 'v>,
    pub(crate) permissions: Sym<'v, 'v>,
    pub(crate) user: Sym<'v, 'v>,
    pub(crate) user_obj: Sym<'v, 'v>,
}

pub struct UnixSecurityTag;

impl<'v> Stateful<'v> for UnixSecurityGlobal<'v> {
    type Tag = UnixSecurityTag;
}

impl<'v> UnixSecurityGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: UnixSecurityTypes {
                unix_identity: builder.register_type(),
                posix_acl: builder.register_type(),
                posix_ace: builder.register_type(),
                permission: Permission::register_type(builder),
            },
            syms: UnixSecuritySyms {
                group: builder.sym("group"),
                group_obj: builder.sym("group_obj"),
                mask: builder.sym("mask"),
                other: builder.sym("other"),
                permissions: builder.sym("permissions"),
                user: builder.sym("user"),
                user_obj: builder.sym("user_obj"),
            },
        }
    }
}

/// State for the `security.nfs4` module.
pub(crate) struct Nfs4SecurityGlobal<'v> {
    pub(crate) types: Nfs4SecurityTypes<'v>,
    pub(crate) syms: Nfs4SecuritySyms<'v>,
}

pub(crate) struct Nfs4SecurityTypes<'v> {
    pub(crate) nfs4_acl: Type<'v, Nfs4AclObject>,
    pub(crate) nfs4_ace: Type<'v, Nfs4AceObject>,
    pub(crate) nfs4_ace_mask: Type<'v, Flags<Nfs4AceMask>>,
    pub(crate) nfs4_ace_flags: Type<'v, Flags<Nfs4AceFlags>>,
}

pub(crate) struct Nfs4SecuritySyms<'v> {
    pub(crate) alarm: Sym<'v, 'v>,
    pub(crate) allow: Sym<'v, 'v>,
    pub(crate) audit: Sym<'v, 'v>,
    pub(crate) deny: Sym<'v, 'v>,
    pub(crate) flags: Sym<'v, 'v>,
    pub(crate) mask: Sym<'v, 'v>,
}

pub struct Nfs4SecurityTag;

impl<'v> Stateful<'v> for Nfs4SecurityGlobal<'v> {
    type Tag = Nfs4SecurityTag;
}

impl<'v> Nfs4SecurityGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            types: Nfs4SecurityTypes {
                nfs4_acl: builder.register_type(),
                nfs4_ace: builder.register_type(),
                nfs4_ace_mask: Nfs4AceMask::register_type(builder),
                nfs4_ace_flags: Nfs4AceFlags::register_type(builder),
            },
            syms: Nfs4SecuritySyms {
                alarm: builder.sym("alarm"),
                allow: builder.sym("allow"),
                audit: builder.sym("audit"),
                deny: builder.sym("deny"),
                flags: builder.sym("flags"),
                mask: builder.sym("mask"),
            },
        }
    }
}

/// State for the `security.macos` module.
pub(crate) struct MacosSecurityGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: MacosSecurityTypes<'v>,
    pub(crate) syms: MacosSecuritySyms<'v>,
}

pub(crate) struct MacosSecurityTypes<'v> {
    pub(crate) macos_acl: Type<'v, MacosAclObject>,
    pub(crate) macos_ace: Type<'v, MacosAceObject>,
    pub(crate) macos_ace_mask: Type<'v, Flags<MacosAceMask>>,
    pub(crate) macos_ace_flags: Type<'v, Flags<MacosAceFlags>>,
}

pub(crate) struct MacosSecuritySyms<'v> {
    pub(crate) allow: Sym<'v, 'v>,
    pub(crate) deny: Sym<'v, 'v>,
    pub(crate) flags: Sym<'v, 'v>,
    pub(crate) mask: Sym<'v, 'v>,
}

pub struct MacosSecurityTag;

impl<'v> Stateful<'v> for MacosSecurityGlobal<'v> {
    type Tag = MacosSecurityTag;
}

impl<'v> MacosSecurityGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: MacosSecurityTypes {
                macos_acl: builder.register_type(),
                macos_ace: builder.register_type(),
                macos_ace_mask: MacosAceMask::register_type(builder),
                macos_ace_flags: MacosAceFlags::register_type(builder),
            },
            syms: MacosSecuritySyms {
                allow: builder.sym("allow"),
                deny: builder.sym("deny"),
                flags: builder.sym("flags"),
                mask: builder.sym("mask"),
            },
        }
    }
}

/// State for the `security.windows` module.
pub(crate) struct WindowsSecurityGlobal<'v> {
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) types: WindowsSecurityTypes<'v>,
    pub(crate) syms: WindowsSecuritySyms<'v>,
}

pub(crate) struct WindowsSecurityTypes<'v> {
    pub(crate) access_mask: Type<'v, Flags<AccessMask>>,
    pub(crate) ace_flags: Type<'v, Flags<AceFlags>>,
    pub(crate) sec_desc_control: Type<'v, Flags<SecDescControl>>,
    pub(crate) sec_info: Type<'v, Flags<SecInfo>>,
    pub(crate) token_group_attributes: Type<'v, Flags<TokenGroupAttributes>>,
    pub(crate) acl: Type<'v, Acl>,
    pub(crate) ace: Type<'v, Ace>,
    pub(crate) sec_desc: Type<'v, SecDesc>,
    pub(crate) sid: Type<'v, Sid>,
    pub(crate) sid_name: Type<'v, SidName>,
    pub(crate) token_group: Type<'v, TokenGroup>,
    pub(crate) token_info: Type<'v, TokenInfo>,
}

pub(crate) struct WindowsSecuritySyms<'v> {
    /// `int`, the raw-bits field every `AccessMask` subtype must expose.
    pub(crate) int: Sym<'v, 'v>,
    // Declarative ACE spec keys.
    pub(crate) allow: Sym<'v, 'v>,
    pub(crate) deny: Sym<'v, 'v>,
    pub(crate) audit: Sym<'v, 'v>,
    pub(crate) mask: Sym<'v, 'v>,
    pub(crate) flags: Sym<'v, 'v>,
    pub(crate) object_type: Sym<'v, 'v>,
    pub(crate) inherited_object_type: Sym<'v, 'v>,
    pub(crate) callback: Sym<'v, 'v>,
    pub(crate) application_data: Sym<'v, 'v>,
    pub(crate) successful: Sym<'v, 'v>,
    pub(crate) failed: Sym<'v, 'v>,
    // ACL options.
    pub(crate) revision: Sym<'v, 'v>,
    pub(crate) basic: Sym<'v, 'v>,
    pub(crate) directory_service: Sym<'v, 'v>,
    // Security descriptor components.
    pub(crate) owner: Sym<'v, 'v>,
    pub(crate) group: Sym<'v, 'v>,
    pub(crate) dacl: Sym<'v, 'v>,
    pub(crate) sacl: Sym<'v, 'v>,
    pub(crate) owner_defaulted: Sym<'v, 'v>,
    pub(crate) group_defaulted: Sym<'v, 'v>,
    pub(crate) dacl_present: Sym<'v, 'v>,
    pub(crate) dacl_defaulted: Sym<'v, 'v>,
    pub(crate) dacl_auto_inherit_required: Sym<'v, 'v>,
    pub(crate) dacl_auto_inherited: Sym<'v, 'v>,
    pub(crate) dacl_protected: Sym<'v, 'v>,
    pub(crate) sacl_present: Sym<'v, 'v>,
    pub(crate) sacl_defaulted: Sym<'v, 'v>,
    pub(crate) sacl_auto_inherit_required: Sym<'v, 'v>,
    pub(crate) sacl_auto_inherited: Sym<'v, 'v>,
    pub(crate) sacl_protected: Sym<'v, 'v>,
    pub(crate) rm_control: Sym<'v, 'v>,
    /// The symbols naming well-known Windows SIDs.
    pub(crate) well_known_sids: WellKnownSids<'v>,
}

pub struct WindowsSecurityTag;

impl<'v> Stateful<'v> for WindowsSecurityGlobal<'v> {
    type Tag = WindowsSecurityTag;
}

impl<'v> WindowsSecurityGlobal<'v> {
    pub(crate) fn new(builder: &mut Register<'v>, local: LocalKey<'v, Local>) -> Self {
        Self {
            local,
            types: WindowsSecurityTypes {
                access_mask: AccessMask::register_type(builder),
                ace_flags: AceFlags::register_type(builder),
                sec_desc_control: SecDescControl::register_type(builder),
                sec_info: SecInfo::register_type(builder),
                token_group_attributes: TokenGroupAttributes::register_type(builder),
                acl: builder.register_type(),
                ace: builder.register_type(),
                sec_desc: builder.register_type(),
                sid: builder.register_type(),
                sid_name: builder.register_type(),
                token_group: builder.register_type(),
                token_info: builder.register_type(),
            },
            syms: WindowsSecuritySyms {
                int: builder.sym("int"),
                allow: builder.sym("allow"),
                deny: builder.sym("deny"),
                audit: builder.sym("audit"),
                mask: builder.sym("mask"),
                flags: builder.sym("flags"),
                object_type: builder.sym("object_type"),
                inherited_object_type: builder.sym("inherited_object_type"),
                callback: builder.sym("callback"),
                application_data: builder.sym("application_data"),
                successful: builder.sym("successful"),
                failed: builder.sym("failed"),
                revision: builder.sym("revision"),
                basic: builder.sym("BASIC"),
                directory_service: builder.sym("DIRECTORY_SERVICE"),
                owner: builder.sym("owner"),
                group: builder.sym("group"),
                dacl: builder.sym("dacl"),
                sacl: builder.sym("sacl"),
                owner_defaulted: builder.sym("owner_defaulted"),
                group_defaulted: builder.sym("group_defaulted"),
                dacl_present: builder.sym("dacl_present"),
                dacl_defaulted: builder.sym("dacl_defaulted"),
                dacl_auto_inherit_required: builder.sym("dacl_auto_inherit_required"),
                dacl_auto_inherited: builder.sym("dacl_auto_inherited"),
                dacl_protected: builder.sym("dacl_protected"),
                sacl_present: builder.sym("sacl_present"),
                sacl_defaulted: builder.sym("sacl_defaulted"),
                sacl_auto_inherit_required: builder.sym("sacl_auto_inherit_required"),
                sacl_auto_inherited: builder.sym("sacl_auto_inherited"),
                sacl_protected: builder.sym("sacl_protected"),
                rm_control: builder.sym("rm_control"),
                well_known_sids: WellKnownSids::new(builder),
            },
        }
    }
}

/// Lazy setup tag for the `shlex` module, which has no state of its own.
pub struct ShlexTag;

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{ConsoleOverride, ansi_enabled};

    #[test]
    fn ansi_policy_respects_terminal_and_color_environment() {
        assert!(ansi_enabled(true, None, None));
        assert!(!ansi_enabled(false, None, None));
        assert!(ansi_enabled(true, None, Some(OsStr::new(""))));
        assert!(!ansi_enabled(true, None, Some(OsStr::new("1"))));
        assert!(ansi_enabled(
            false,
            Some(OsStr::new("1")),
            Some(OsStr::new(""))
        ));
        assert!(!ansi_enabled(true, Some(OsStr::new("0")), None));
    }

    #[test]
    fn console_override_parses_nothing_when_unset() {
        let ov = ConsoleOverride::parse(None);
        assert_eq!(ov.tty, None);
        assert_eq!(ov.rows, None);
        assert_eq!(ov.cols, None);
        assert_eq!(ov.style, None);
    }

    #[test]
    fn console_override_parses_every_key() {
        let ov = ConsoleOverride::parse(Some("tty=false,cols=120,rows=40,style=true"));
        assert_eq!(ov.tty, Some(false));
        assert_eq!(ov.rows, Some(40));
        assert_eq!(ov.cols, Some(120));
        assert_eq!(ov.style, Some(true));
    }

    #[test]
    fn console_override_ignores_unknown_keys_and_bad_values() {
        let ov = ConsoleOverride::parse(Some("wat=1,tty=maybe,cols=wide,rows=40"));
        assert_eq!(ov.tty, None);
        assert_eq!(ov.cols, None);
        assert_eq!(ov.rows, Some(40));
        assert_eq!(ov.style, None);
    }

    #[test]
    fn console_override_tolerates_whitespace_and_empty_entries() {
        let ov = ConsoleOverride::parse(Some(" tty = false , , cols=80 "));
        assert_eq!(ov.tty, Some(false));
        assert_eq!(ov.cols, Some(80));
    }
}
