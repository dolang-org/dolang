use std::{
    cell::{Cell, RefCell},
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
        SysErrorObject, TimedOutError,
    },
    fs::{
        attrs::Attrs,
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
    security::{TokenInfo, UnixInfo},
    shell::{Stderr, Stdin, Stdout, Vfs},
    sys::{CpuInfo, OsInfo},
    time::{DateTime, Duration},
};

pub(crate) struct Types<'v> {
    pub(crate) path: Type<'v, Path>,
    pub(crate) unix_path: Type<'v, UnixPath>,
    pub(crate) windows_path: Type<'v, WindowsPath>,
    pub(crate) attrs: Type<'v, Attrs>,
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
    pub(crate) unix_info: Type<'v, UnixInfo>,
    pub(crate) token_info: Type<'v, TokenInfo>,
    pub(crate) sys_error: Type<'v, SysErrorObject<SysError>>,
    pub(crate) not_found: Type<'v, SysErrorObject<NotFoundError>>,
    pub(crate) permission_denied: Type<'v, SysErrorObject<PermissionDeniedError>>,
    pub(crate) already_exists: Type<'v, SysErrorObject<AlreadyExistsError>>,
    pub(crate) timed_out: Type<'v, SysErrorObject<TimedOutError>>,
    pub(crate) proc_error: Type<'v, ProcError>,
    pub(crate) pipe_receiver: Type<'v, PipeReceiver>,
    pub(crate) pipe_sender: Type<'v, PipeSender>,
    pub(crate) vfs: Type<'v, Vfs>,
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
    pub(crate) namespace: Sym<'v, 'v>,
    pub(crate) socket: Sym<'v, 'v>,
    pub(crate) stderr: Sym<'v, 'v>,
    pub(crate) stdin: Sym<'v, 'v>,
    pub(crate) stdout: Sym<'v, 'v>,
    pub(crate) stream: Sym<'v, 'v>,
    pub(crate) symlink: Sym<'v, 'v>,
    pub(crate) unknown: Sym<'v, 'v>,
    pub(crate) follow: Sym<'v, 'v>,
    pub(crate) group: Sym<'v, 'v>,
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
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
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

        Self {
            terminal: Terminal {
                writer: Mutex::new(Box::pin(stderr())),
                redirected: Cell::new(false),
                stdout_is_terminal: std::io::stdout().is_terminal(),
            },
            types: Types {
                file: builder.register_type(),
                path,
                unix_path,
                windows_path,
                attrs: builder.register_type(),
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
                unix_info: builder.register_type(),
                token_info: builder.register_type(),
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
                proc_error: builder.register_type(),
                pipe_receiver: builder.register_type(),
                pipe_sender: builder.register_type(),
                vfs: builder.register_type(),
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
                namespace: builder.sym("namespace"),
                socket: builder.sym("SOCKET"),
                stderr: builder.sym("stderr"),
                stdin: builder.sym("stdin"),
                stdout: builder.sym("stdout"),
                stream: builder.sym("stream"),
                symlink: builder.sym("SYMLINK"),
                unknown: builder.sym("UNKNOWN"),
                follow: builder.sym("follow"),
                group: builder.sym("group"),
            },
            local: builder.local(),
            args: RefCell::new(Vec::new()),
            program: RefCell::new(None),
        }
    }
}
