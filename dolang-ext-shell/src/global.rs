use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    io::IsTerminal,
    pin::Pin,
};

use dolang::runtime::{
    Sym, Type,
    strand::LocalKey,
    value::TypeObject,
    vm::{Builder, Stateful},
};
use tokio::{
    io::{AsyncWrite, stderr},
    sync::Mutex,
};

use crate::{
    error::{
        AlreadyExistsError, NotFoundError, PermissionDeniedError, ProcError, SysError,
        SysErrorObject, TimedOutError, UnsupportedError,
    },
    error_code::{CodeObject, Errno, ErrorCode, LinuxErrno, MacosErrno, WinError},
    fs::{
        file::File,
        fs_metadata::FsMetadata,
        metadata::Metadata,
        path::{Path, UnixPath, WindowsPath},
        readdir::{DirEntry, DirEntryIter},
        stream::{StreamEntry, StreamIter},
        xattr::{XattrEntry, XattrIter},
    },
    local::Local,
    pipe_channel::{PipeReceiver, PipeSender},
    program::Program,
    security::{Ace, Acl, Guid, Identity, SecDesc, Sid, SidName, TokenGroup, TokenInfo},
    shell::{Stderr, Stdin, Stdout, Vfs},
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
    pub(crate) dir_entry: Type<'v, DirEntry>,
    pub(crate) dir_entry_iter: Type<'v, DirEntryIter>,
    pub(crate) glob_iter: Type<'v, crate::fs::glob::GlobIter>,
    pub(crate) program: Type<'v, Program>,
    pub(crate) stdin: Type<'v, Stdin>,
    pub(crate) stdout: Type<'v, Stdout>,
    pub(crate) stderr: Type<'v, Stderr>,
    pub(crate) date_time: Type<'v, DateTime>,
    pub(crate) duration: Type<'v, Duration>,
    pub(crate) os_info: Type<'v, OsInfo>,
    pub(crate) cpu_info: Type<'v, CpuInfo>,
    pub(crate) unix_identity: Type<'v, Identity>,
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
    pub(crate) pipe_receiver: Type<'v, PipeReceiver>,
    pub(crate) pipe_sender: Type<'v, PipeSender>,
    pub(crate) vfs: Type<'v, Vfs>,
    pub(crate) text: Type<'v, Text>,
    pub(crate) style: Type<'v, StyleObject>,
}

pub(crate) struct Syms<'v> {
    pub(crate) any: Sym<'v, 'v>,
    pub(crate) block_device: Sym<'v, 'v>,
    pub(crate) char_device: Sym<'v, 'v>,
    pub(crate) chunk: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) dir: Sym<'v, 'v>,
    pub(crate) fifo: Sym<'v, 'v>,
    pub(crate) file: Sym<'v, 'v>,
    pub(crate) line: Sym<'v, 'v>,
    pub(crate) link: Sym<'v, 'v>,
    pub(crate) inherit: Sym<'v, 'v>,
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) revision: Sym<'v, 'v>,
    pub(crate) socket: Sym<'v, 'v>,
    pub(crate) stderr: Sym<'v, 'v>,
    pub(crate) stdin: Sym<'v, 'v>,
    pub(crate) stdout: Sym<'v, 'v>,
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

pub enum ProgramSource {
    Path(std::path::PathBuf),
    Module(String),
}

pub(crate) struct Global<'v> {
    pub(crate) terminal: Terminal,
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
    pub(crate) local: LocalKey<'v, Local>,
    pub(crate) args: RefCell<Vec<String>>,
    pub(crate) program: RefCell<Option<ProgramSource>>,
}

pub(crate) struct Terminal {
    /// The writer, behind an async mutex so it can be held across await
    /// points by concurrent strands without conflict.
    pub(crate) writer: Mutex<Pin<Box<dyn AsyncWrite>>>,
    pub(crate) redirected: Cell<bool>,
    /// Whether stdout was a terminal at startup (cached to avoid repeated
    /// syscalls).
    pub(crate) stdout_is_terminal: bool,
    /// Whether stderr was a terminal at startup.
    pub(crate) stderr_is_terminal: bool,
    /// Whether ANSI styling should be emitted to stderr.
    pub(crate) ansi: bool,
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

        let stderr_is_terminal = std::io::stderr().is_terminal();
        Self {
            terminal: Terminal {
                writer: Mutex::new(Box::pin(stderr())),
                redirected: Cell::new(false),
                stdout_is_terminal: std::io::stdout().is_terminal(),
                stderr_is_terminal,
                ansi: ansi_enabled(
                    stderr_is_terminal,
                    std::env::var_os("FORCE_COLOR").as_deref(),
                    std::env::var_os("NO_COLOR").as_deref(),
                ),
            },
            types: Types {
                file: builder.register_type(),
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
                date_time: builder.register_type::<DateTime>(),
                duration: builder.register_type::<Duration>(),
                os_info: builder.register_type(),
                cpu_info: builder.register_type(),
                unix_identity: builder.register_type(),
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
                pipe_receiver: builder.register_type(),
                pipe_sender: builder.register_type(),
                vfs: builder.register_type(),
                text: builder.register_type(),
                style: builder.register_type(),
            },
            syms: Syms {
                any: builder.sym("ANY"),
                block_device: builder.sym("BLOCK_DEVICE"),
                char_device: builder.sym("CHAR_DEVICE"),
                chunk: builder.sym("CHUNK"),
                close: builder.sym("close"),
                dir: builder.sym("DIR"),
                fifo: builder.sym("FIFO"),
                file: builder.sym("FILE"),
                line: builder.sym("LINE"),
                link: builder.sym("LINK"),
                inherit: builder.sym("INHERIT"),
                namespace: builder.sym("namespace"),
                revision: builder.sym("revision"),
                socket: builder.sym("SOCKET"),
                stderr: builder.sym("stderr"),
                stdin: builder.sym("stdin"),
                stdout: builder.sym("stdout"),
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
            args: RefCell::new(Vec::new()),
            program: RefCell::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::ansi_enabled;

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
}
