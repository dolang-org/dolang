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

use crate::pipe_channel::{PipeReceiver, PipeSender};
use crate::{
    error::{
        AlreadyExistsError, NotFoundError, PermissionDeniedError, ProcError, SysError,
        SysErrorObject, TimedOutError,
    },
    fs::{file::File, path::Path},
    local::Local,
    program::Program,
    sys::{Stderr, Stdin, Stdout},
    time::{DateTime, Duration},
};

#[cfg(unix)]
use crate::container::Vfs;

use crate::fs::readdir::DirEntryIter;

pub(crate) struct Types<'v> {
    pub(crate) path: Type<'v, Path>,
    pub(crate) file: Type<'v, File>,
    pub(crate) dir_entry_iter: Type<'v, DirEntryIter>,
    pub(crate) glob_iter: Type<'v, crate::fs::glob::GlobIter>,
    pub(crate) program: Type<'v, Program>,
    pub(crate) stdin: Type<'v, Stdin>,
    pub(crate) stdout: Type<'v, Stdout>,
    pub(crate) stderr: Type<'v, Stderr>,
    pub(crate) date_time: Type<'v, DateTime>,
    pub(crate) duration: Type<'v, Duration>,
    pub(crate) sys_error: Type<'v, SysErrorObject<SysError>>,
    pub(crate) not_found: Type<'v, SysErrorObject<NotFoundError>>,
    pub(crate) permission_denied: Type<'v, SysErrorObject<PermissionDeniedError>>,
    pub(crate) already_exists: Type<'v, SysErrorObject<AlreadyExistsError>>,
    pub(crate) timed_out: Type<'v, SysErrorObject<TimedOutError>>,
    pub(crate) proc_error: Type<'v, ProcError>,
    pub(crate) pipe_receiver: Type<'v, PipeReceiver>,
    pub(crate) pipe_sender: Type<'v, PipeSender>,
    #[cfg(unix)]
    pub(crate) vfs: Type<'v, Vfs>,
}

pub(crate) struct Syms<'v> {
    pub(crate) accessed: Sym<'v, 'v>,
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) created: Sym<'v, 'v>,
    pub(crate) dir: Sym<'v, 'v>,
    pub(crate) file: Sym<'v, 'v>,
    pub(crate) len: Sym<'v, 'v>,
    pub(crate) modified: Sym<'v, 'v>,
    pub(crate) path: Sym<'v, 'v>,
    pub(crate) record: Sym<'v, 'v>,
    pub(crate) stderr: Sym<'v, 'v>,
    pub(crate) stdin: Sym<'v, 'v>,
    pub(crate) stdout: Sym<'v, 'v>,
    pub(crate) symlink: Sym<'v, 'v>,
    pub(crate) ty: Sym<'v, 'v>,
    pub(crate) unknown: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) mode: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) follow: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) group: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) dev: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) ino: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) nlink: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) uid: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) gid: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) rdev: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) blksize: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) blocks: Sym<'v, 'v>,
    pub(crate) fifo: Sym<'v, 'v>,
    pub(crate) char_device: Sym<'v, 'v>,
    pub(crate) block_device: Sym<'v, 'v>,
    pub(crate) socket: Sym<'v, 'v>,
    #[cfg(unix)]
    pub(crate) unix_socket: Sym<'v, 'v>,
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

        Self {
            terminal: Terminal {
                writer: Mutex::new(Box::pin(stderr())),
                redirected: Cell::new(false),
                stdout_is_terminal: std::io::stdout().is_terminal(),
            },
            types: Types {
                file: builder.register_type(),
                path: builder.register_type(),
                dir_entry_iter: builder.register_type(),
                glob_iter: builder.register_type(),
                program: builder.register_type(),
                stdin: builder.register_type(),
                stdout: builder.register_type(),
                stderr: builder.register_type(),
                date_time: builder.register_type::<DateTime>(),
                duration: builder.register_type::<Duration>(),
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
                    .build(),
                proc_error: builder.register_type(),
                pipe_receiver: builder.register_type(),
                pipe_sender: builder.register_type(),
                #[cfg(unix)]
                vfs: builder.register_type(),
            },
            syms: Syms {
                accessed: builder.sym("accessed"),
                close: builder.sym("close"),
                created: builder.sym("created"),
                dir: builder.sym("dir"),
                file: builder.sym("file"),
                len: builder.sym("len"),
                modified: builder.sym("modified"),
                path: builder.sym("path"),
                record: builder.sym("record"),
                stderr: builder.sym("stderr"),
                stdin: builder.sym("stdin"),
                stdout: builder.sym("stdout"),
                symlink: builder.sym("symlink"),
                ty: builder.sym("type"),
                unknown: builder.sym("unknown"),
                #[cfg(unix)]
                mode: builder.sym("mode"),
                #[cfg(unix)]
                follow: builder.sym("follow"),
                #[cfg(unix)]
                group: builder.sym("group"),
                #[cfg(unix)]
                dev: builder.sym("dev"),
                #[cfg(unix)]
                ino: builder.sym("ino"),
                #[cfg(unix)]
                nlink: builder.sym("nlink"),
                #[cfg(unix)]
                uid: builder.sym("uid"),
                #[cfg(unix)]
                gid: builder.sym("gid"),
                #[cfg(unix)]
                rdev: builder.sym("rdev"),
                #[cfg(unix)]
                blksize: builder.sym("blksize"),
                #[cfg(unix)]
                blocks: builder.sym("blocks"),
                fifo: builder.sym("fifo"),
                char_device: builder.sym("char_device"),
                block_device: builder.sym("block_device"),
                socket: builder.sym("socket"),
                #[cfg(unix)]
                unix_socket: builder.sym("unix_socket"),
            },
            local: builder.local(),
            args: RefCell::new(Vec::new()),
            program: RefCell::new(None),
        }
    }
}
