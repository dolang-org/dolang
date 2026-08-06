use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    io::IsTerminal,
    pin::Pin,
    rc::Rc,
};

use dolang::runtime::{
    Sym, Type,
    object::{FlagLike, Flags},
    strand::{LocalKey, LocalRootKey},
    value::TypeObject,
    vm::{Builder, Stateful},
};
use tokio::{
    io::{self as tio, AsyncWrite, stderr},
    sync::Mutex,
};

use crate::{
    console::{Console, DefaultOutput, HostConsole, SinkConsole, SubConsole},
    error::{
        AlreadyExistsError, NotFoundError, PermissionDeniedError, ProcError, SysError,
        SysErrorObject, TimedOutError, UnsupportedError,
    },
    error_code::{CodeObject, Errno, ErrorCode, FreeBsdErrno, LinuxErrno, MacosErrno, WinError},
    fs::{
        file::File,
        file_lock::FileLock,
        fs_metadata::FsMetadata,
        metadata::Metadata,
        path::{Path, UnixPath, WindowsPath},
        readdir::{DirEntry, DirEntryIter},
        stream::{StreamEntry, StreamIter},
        xattr::{XattrEntry, XattrIter},
    },
    geometry::{Geometry, HostGeometry},
    local::Local,
    pipe_channel::{PipeReceiver, PipeSender},
    proc::Capture,
    program::Program,
    security::{
        AccessMask, Ace, Acl, Guid, Identity, PosixAceObject, PosixAclObject, SecDesc, Sid,
        SidName, TokenGroup, TokenInfo,
    },
    shell::{Stderr, Stdin, Stdout, Vfs},
    shell_args::ArgsData,
    sys::{CpuInfo, OsInfo},
    term::{StyleObject, Text},
    time::{DateTime, Duration},
};

pub(crate) struct Types<'v> {
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
    pub(crate) program: Type<'v, Program>,
    pub(crate) stdin: Type<'v, Stdin>,
    pub(crate) stdout: Type<'v, Stdout>,
    pub(crate) stderr: Type<'v, Stderr>,
    pub(crate) console: Type<'v, Console>,
    pub(crate) host_console: Type<'v, HostConsole>,
    pub(crate) sink_console: Type<'v, SinkConsole>,
    pub(crate) sub_console: Type<'v, SubConsole>,
    pub(crate) default: Type<'v, DefaultOutput>,
    pub(crate) geometry: Type<'v, Geometry>,
    pub(crate) host_geometry: Type<'v, HostGeometry>,
    pub(crate) date_time: Type<'v, DateTime>,
    pub(crate) duration: Type<'v, Duration>,
    pub(crate) os_info: Type<'v, OsInfo>,
    pub(crate) cpu_info: Type<'v, CpuInfo>,
    pub(crate) unix_identity: Type<'v, Identity>,
    pub(crate) posix_acl: Type<'v, PosixAclObject>,
    pub(crate) posix_ace: Type<'v, PosixAceObject>,
    pub(crate) guid: Type<'v, Guid>,
    pub(crate) acl: Type<'v, Acl>,
    pub(crate) ace: Type<'v, Ace>,
    pub(crate) sec_desc: Type<'v, SecDesc>,
    pub(crate) sid: Type<'v, Sid>,
    pub(crate) sid_name: Type<'v, SidName>,
    pub(crate) token_group: Type<'v, TokenGroup>,
    pub(crate) token_info: Type<'v, TokenInfo>,
    pub(crate) error_code: Type<'v, CodeObject<ErrorCode>>,
    pub(crate) errno: Type<'v, CodeObject<Errno>>,
    pub(crate) freebsd_errno: Type<'v, CodeObject<FreeBsdErrno>>,
    pub(crate) linux_errno: Type<'v, CodeObject<LinuxErrno>>,
    pub(crate) macos_errno: Type<'v, CodeObject<MacosErrno>>,
    pub(crate) win_error: Type<'v, CodeObject<WinError>>,
    pub(crate) sys_error: Type<'v, SysErrorObject<SysError>>,
    pub(crate) not_found: Type<'v, SysErrorObject<NotFoundError>>,
    pub(crate) permission_denied: Type<'v, SysErrorObject<PermissionDeniedError>>,
    pub(crate) already_exists: Type<'v, SysErrorObject<AlreadyExistsError>>,
    pub(crate) timed_out: Type<'v, SysErrorObject<TimedOutError>>,
    pub(crate) unsupported: Type<'v, SysErrorObject<UnsupportedError>>,
    pub(crate) proc_error: Type<'v, ProcError>,
    pub(crate) capture: Type<'v, Capture>,
    pub(crate) pipe_receiver: Type<'v, PipeReceiver>,
    pub(crate) pipe_sender: Type<'v, PipeSender>,
    pub(crate) vfs: Type<'v, Vfs>,
    pub(crate) text: Type<'v, Text>,
    pub(crate) style: Type<'v, StyleObject>,
    pub(crate) access_mask: Type<'v, Flags<AccessMask>>,
}

pub(crate) struct Syms<'v> {
    pub(crate) any: Sym<'v, 'v>,
    pub(crate) code: Sym<'v, 'v>,
    pub(crate) block_device: Sym<'v, 'v>,
    pub(crate) char_device: Sym<'v, 'v>,
    pub(crate) chunk: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) write: Sym<'v, 'v>,
    pub(crate) writeln: Sym<'v, 'v>,
    pub(crate) flush: Sym<'v, 'v>,
    pub(crate) can_style: Sym<'v, 'v>,
    pub(crate) is_tty: Sym<'v, 'v>,
    pub(crate) geometry: Sym<'v, 'v>,
    pub(crate) dir: Sym<'v, 'v>,
    pub(crate) fifo: Sym<'v, 'v>,
    pub(crate) file: Sym<'v, 'v>,
    pub(crate) line: Sym<'v, 'v>,
    pub(crate) link: Sym<'v, 'v>,
    pub(crate) inherit: Sym<'v, 'v>,
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) namespace_system: Sym<'v, 'v>,
    pub(crate) namespace_user: Sym<'v, 'v>,
    pub(crate) revision: Sym<'v, 'v>,
    pub(crate) socket: Sym<'v, 'v>,
    pub(crate) stderr: Sym<'v, 'v>,
    pub(crate) stdin: Sym<'v, 'v>,
    pub(crate) stdout: Sym<'v, 'v>,
    pub(crate) policy: Sym<'v, 'v>,
    pub(crate) signal: Sym<'v, 'v>,
    pub(crate) grace: Sym<'v, 'v>,
    pub(crate) force: Sym<'v, 'v>,
    pub(crate) stream: Sym<'v, 'v>,
    pub(crate) symlink: Sym<'v, 'v>,
    pub(crate) target: Sym<'v, 'v>,
    pub(crate) unknown: Sym<'v, 'v>,
    pub(crate) group: Sym<'v, 'v>,
    pub(crate) join: Sym<'v, 'v>,
    pub(crate) owner: Sym<'v, 'v>,
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
}

#[derive(Clone)]
pub enum ProgramSource {
    Path(std::path::PathBuf),
    Module(String),
}

pub(crate) struct Global<'v> {
    pub(crate) terminal: Terminal,
    pub(crate) stdio: Stdio,
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
    pub(crate) local: LocalKey<'v, Local>,
    /// The console installed by an enclosing `term.capture`, or `nil` for none.
    ///
    /// A strand-local root rather than a `Local` field because it holds a GC
    /// value; it is duplicated into derived strands at spawn, so a capture
    /// covers whatever the block spawns.
    pub(crate) capture: LocalRootKey<'v>,
    pub(crate) args: RefCell<ArgsData>,
    pub(crate) program: RefCell<Option<ProgramSource>>,
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
    /// `HostConsole::is_tty`, `console::ansi`'s tty-detection fallback, and
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
        let error_code = builder.register_type::<CodeObject<ErrorCode>>();
        let errno = builder
            .build_type::<CodeObject<Errno>>((), ())
            .nominal_supertype(error_code)
            .build();
        let sys_error = builder
            .build_type::<SysErrorObject<SysError>>((), ())
            .nominal_supertype(TypeObject::RuntimeError)
            .build();

        let path = builder.register_type::<Path>();
        let unix_path = builder
            .build_type::<UnixPath>((), ())
            .nominal_supertype(path)
            .build();
        let windows_path = builder
            .build_type::<WindowsPath>((), ())
            .nominal_supertype(path)
            .build();

        let console = builder.register_type::<Console>();
        let host_console = builder
            .build_type::<HostConsole>((), ())
            .nominal_supertype(console)
            .build();
        let sink_console = builder
            .build_type::<SinkConsole>((), ())
            .nominal_supertype(console)
            .build();
        let sub_console = builder
            .build_type::<SubConsole>((), ())
            .nominal_supertype(console)
            .build();
        let default = builder
            .build_type::<DefaultOutput>((), ())
            .nominal_supertype(console)
            .build();

        let geometry = builder.register_type::<Geometry>();
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
                file: builder.register_type(),
                file_lock: builder.register_type(),
                path,
                unix_path,
                windows_path,
                xattr_entry: builder.register_type(),
                xattr_iter: builder.register_type(),
                stream_entry: builder.register_type(),
                stream_iter: builder.register_type(),
                metadata: builder.register_type(),
                fs_metadata: builder.register_type(),
                dir_entry: builder.register_type(),
                dir_entry_iter: builder.register_type(),
                glob_iter: builder.register_type(),
                program: builder.register_type(),
                stdin: builder.register_type(),
                stdout: builder.register_type(),
                stderr: builder.register_type(),
                console,
                host_console,
                sink_console,
                sub_console,
                default,
                geometry,
                host_geometry,
                date_time: builder.register_type::<DateTime>(),
                duration: builder.register_type::<Duration>(),
                os_info: builder.register_type(),
                cpu_info: builder.register_type(),
                unix_identity: builder.register_type(),
                posix_acl: builder.register_type(),
                posix_ace: builder.register_type(),
                guid: builder.register_type(),
                acl: builder.register_type(),
                ace: builder.register_type(),
                sec_desc: builder.register_type(),
                sid: builder.register_type(),
                sid_name: builder.register_type(),
                token_group: builder.register_type(),
                token_info: builder.register_type(),
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
                proc_error: builder.register_type(),
                capture: builder.register_type(),
                pipe_receiver: builder.register_type(),
                pipe_sender: builder.register_type(),
                vfs: builder.register_type(),
                text: builder.register_type(),
                style: builder.register_type(),
                access_mask: AccessMask::register_type(builder),
            },
            syms: Syms {
                any: builder.sym("ANY"),
                code: builder.sym("code"),
                block_device: builder.sym("BLOCK_DEVICE"),
                char_device: builder.sym("CHAR_DEVICE"),
                chunk: builder.sym("CHUNK"),
                close: builder.sym("close"),
                write: builder.sym("write"),
                writeln: builder.sym("writeln"),
                flush: builder.sym("flush"),
                can_style: builder.sym("can_style"),
                is_tty: builder.sym("is_tty"),
                geometry: builder.sym("geometry"),
                dir: builder.sym("DIR"),
                fifo: builder.sym("FIFO"),
                file: builder.sym("FILE"),
                line: builder.sym("LINE"),
                link: builder.sym("LINK"),
                inherit: builder.sym("INHERIT"),
                namespace: builder.sym("namespace"),
                namespace_system: builder.sym("SYSTEM"),
                namespace_user: builder.sym("USER"),
                revision: builder.sym("revision"),
                socket: builder.sym("SOCKET"),
                stderr: builder.sym("stderr"),
                stdin: builder.sym("stdin"),
                stdout: builder.sym("stdout"),
                policy: builder.sym("policy"),
                signal: builder.sym("signal"),
                grace: builder.sym("grace"),
                force: builder.sym("force"),
                stream: builder.sym("stream"),
                symlink: builder.sym("SYMLINK"),
                target: builder.sym("TARGET"),
                unknown: builder.sym("UNKNOWN"),
                group: builder.sym("group"),
                join: builder.sym("join"),
                owner: builder.sym("owner"),
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
            },
            local: builder.local(),
            capture: builder.local_root(),
            args: RefCell::new(Rc::from([])),
            program: RefCell::new(None),
        }
    }
}

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

    #[test]
    fn console_override_style_key_takes_precedence_over_no_color_force_color() {
        // The override's `style` key is consulted before `ansi_enabled` is
        // even called — this proves it wins regardless of what
        // FORCE_COLOR/NO_COLOR say, per DOLANG_CONSOLE's contract.
        let ov = ConsoleOverride::parse(Some("style=false"));
        let effective = match ov.style {
            Some(style) => style,
            None => ansi_enabled(true, Some(OsStr::new("1")), None),
        };
        assert!(!effective);

        let ov = ConsoleOverride::parse(Some("style=true"));
        let effective = match ov.style {
            Some(style) => style,
            None => ansi_enabled(false, None, Some(OsStr::new("1"))),
        };
        assert!(effective);
    }
}
