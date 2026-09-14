use std::{io, marker::PhantomData};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::object::fmt;
use dolang_vfs::{
    error::{Error as VfsError, ErrorKind},
    target::OperatingSystem,
};

use dolang::runtime::{
    AllocExt, Args, Error, Instance, Object, Output, Result, Slot, Strand, Type,
    object::TypeBuilder, unpack, value::TypeObject,
};

use crate::{error_code, global::ErrorGlobal};

pub(crate) struct SysError;
pub(crate) struct InvalidInputError;
pub(crate) struct NotFoundError;
pub(crate) struct PermissionDeniedError;
pub(crate) struct AlreadyExistsError;
pub(crate) struct TimedOutError;
pub(crate) struct UnsupportedError;

macro_rules! sys_error_types {
    ($($type:ident),+ $(,)?) => {
        $(pub(crate) struct $type;)+

        $(impl<'v> SysErrorType<'v> for $type {
            const NAME: &'v str = stringify!($type);
        })+
    };
}

sys_error_types!(
    ConnectionRefusedError,
    ConnectionResetError,
    HostUnreachableError,
    NetworkUnreachableError,
    ConnectionAbortedError,
    NotConnectedError,
    AddrInUseError,
    AddrNotAvailableError,
    NetworkDownError,
    BrokenPipeError,
    WouldBlockError,
    NotADirectoryError,
    IsADirectoryError,
    DirectoryNotEmptyError,
    ReadOnlyFilesystemError,
    StaleNetworkFileHandleError,
    WriteZeroError,
    StorageFullError,
    NotSeekableError,
    QuotaExceededError,
    FileTooLargeError,
    ResourceBusyError,
    ExecutableFileBusyError,
    DeadlockError,
    CrossesDevicesError,
    TooManyLinksError,
    InvalidFilenameError,
    ArgumentListTooLongError,
    InvalidDataError,
    InterruptedError,
    UnexpectedEofError,
    OutOfMemoryError,
);

pub(crate) struct SysErrorObject<T>(PhantomData<T>);

impl<T> Default for SysErrorObject<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct SysErrorAnnex {
    pub(crate) message: String,
    pub(crate) system_code: Option<(OperatingSystem, i32)>,
}

impl SysErrorAnnex {
    fn message(&self) -> String {
        let Some((operating_system, code)) = self.system_code else {
            return self.message.clone();
        };
        let Some(name) = error_code::system_code_name(operating_system, code) else {
            return self.message.clone();
        };
        format!("{} ({name})", self.message)
    }
}

trait SysErrorType<'v>: Sized + 'v {
    const NAME: &'v str;
}

impl<'v, T: SysErrorType<'v>> Object<'v> for SysErrorObject<T> {
    const NAME: &'v str = T::NAME;
    const MODULE: &'v str = "sys";
    type Annex = SysErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let code_sym = strand.force_state::<ErrorGlobal<'v>>().syms.code;
        let ([message], [code]) = unpack!(strand, args, 1, 0, code_sym = None)?;
        let message = message
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "message: expected Str"))?
            .to_string();
        let system_code = match code {
            Some(code) => Some(error_code::extract_system_code(strand, &code).ok_or_else(
                || Error::type_error(strand, "code: expected a concrete sys.ErrorCode"),
            )?),
            None => None,
        };
        this.create_with_annex(
            strand,
            SysErrorObject::default(),
            SysErrorAnnex {
                message,
                system_code,
            },
            out,
        );
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("code", |this, strand, out| {
            if let Some((operating_system, code)) = this.annex().system_code {
                error_code::create_system_code(strand, operating_system, code, out);
            }
            Ok(())
        })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<sys.{} ", T::NAME)?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> SysErrorType<'v> for SysError {
    const NAME: &'v str = "Error";
}

impl<'v> SysErrorType<'v> for InvalidInputError {
    const NAME: &'v str = "InvalidInputError";
}

impl<'v> SysErrorType<'v> for NotFoundError {
    const NAME: &'v str = "NotFoundError";
}

impl<'v> SysErrorType<'v> for PermissionDeniedError {
    const NAME: &'v str = "PermissionDeniedError";
}

impl<'v> SysErrorType<'v> for AlreadyExistsError {
    const NAME: &'v str = "AlreadyExistsError";
}

impl<'v> SysErrorType<'v> for TimedOutError {
    const NAME: &'v str = "TimedOutError";
}

impl<'v> SysErrorType<'v> for UnsupportedError {
    const NAME: &'v str = "UnsupportedError";
}

fn create_sys_error<'v, 's, T: SysErrorType<'v>>(
    strand: &mut Strand<'v, 's>,
    ty: Type<'v, SysErrorObject<T>>,
    message: String,
    system_code: Option<(OperatingSystem, i32)>,
) -> Error<'v, 's> {
    Error::object_with_annex(
        strand,
        ty,
        SysErrorObject::<T>::default(),
        SysErrorAnnex {
            message,
            system_code,
        },
    )
}

pub(crate) struct ProcError;

pub(crate) struct ProcErrorAnnex {
    pub(crate) name: String,
    pub(crate) status: dolang_vfs::process::ProcessStatus,
    pub(crate) operating_system: OperatingSystem,
}

impl ProcErrorAnnex {
    fn message(&self) -> String {
        if let Some(signal) = self.status.signal() {
            return format!("{} terminated by signal: {}", self.name, signal);
        }

        if let Some(code) = self.status.code() {
            format!("{} exited with status: {}", self.name, code)
        } else {
            format!("{} exited unsuccessfully", self.name)
        }
    }
}

impl<'v> Object<'v> for ProcError {
    const NAME: &'v str = "Error";
    const MODULE: &'v str = "proc";
    type Annex = ProcErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let mut builder =
            builder
                .nominal_supertype(TypeObject::RuntimeError)
                .get("rc", |this, strand, out| {
                    if let Some(code) = this.annex().status.code() {
                        Output::set(strand, out, i64::from(code));
                    }
                    Ok(())
                });
        let signal = builder.sym("signal");
        builder.get("signal", move |this, strand, out| {
            if matches!(this.annex().operating_system, OperatingSystem::Windows) {
                return Err(Error::field(strand, signal));
            }
            if let Some(signal) = this.annex().status.signal() {
                Output::set(strand, out, i64::from(signal));
            }
            Ok(())
        })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<proc.Error ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

pub(crate) fn io_error<'v, 's>(strand: &mut Strand<'v, 's>, error: io::Error) -> Error<'v, 's> {
    vfs_error(strand, error.into())
}

pub(crate) fn vfs_error<'v, 's>(strand: &mut Strand<'v, 's>, error: VfsError) -> Error<'v, 's> {
    sys_error(strand, error)
}

fn sys_error<'v, 's>(strand: &mut Strand<'v, 's>, error: VfsError) -> Error<'v, 's> {
    let global = strand.force_state::<ErrorGlobal<'v>>();
    let message = error.message().to_owned();
    let system_code = error
        .system_code()
        .map(|code| (code.operating_system(), code.raw()));
    match error.kind() {
        ErrorKind::Other => create_sys_error(strand, global.types.sys_error, message, system_code),
        ErrorKind::InvalidInput => {
            create_sys_error(strand, global.types.invalid_input, message, system_code)
        }
        ErrorKind::NotFound => {
            create_sys_error(strand, global.types.not_found, message, system_code)
        }
        ErrorKind::PermissionDenied => {
            create_sys_error(strand, global.types.permission_denied, message, system_code)
        }
        ErrorKind::AlreadyExists => {
            create_sys_error(strand, global.types.already_exists, message, system_code)
        }
        ErrorKind::TimedOut => {
            create_sys_error(strand, global.types.timed_out, message, system_code)
        }
        ErrorKind::Unsupported => {
            create_sys_error(strand, global.types.unsupported, message, system_code)
        }
        ErrorKind::ConnectionRefused => create_sys_error(
            strand,
            global.types.connection_refused,
            message,
            system_code,
        ),
        ErrorKind::ConnectionReset => {
            create_sys_error(strand, global.types.connection_reset, message, system_code)
        }
        ErrorKind::HostUnreachable => {
            create_sys_error(strand, global.types.host_unreachable, message, system_code)
        }
        ErrorKind::NetworkUnreachable => create_sys_error(
            strand,
            global.types.network_unreachable,
            message,
            system_code,
        ),
        ErrorKind::ConnectionAborted => create_sys_error(
            strand,
            global.types.connection_aborted,
            message,
            system_code,
        ),
        ErrorKind::NotConnected => {
            create_sys_error(strand, global.types.not_connected, message, system_code)
        }
        ErrorKind::AddrInUse => {
            create_sys_error(strand, global.types.addr_in_use, message, system_code)
        }
        ErrorKind::AddrNotAvailable => create_sys_error(
            strand,
            global.types.addr_not_available,
            message,
            system_code,
        ),
        ErrorKind::NetworkDown => {
            create_sys_error(strand, global.types.network_down, message, system_code)
        }
        ErrorKind::BrokenPipe => {
            create_sys_error(strand, global.types.broken_pipe, message, system_code)
        }
        ErrorKind::WouldBlock => {
            create_sys_error(strand, global.types.would_block, message, system_code)
        }
        ErrorKind::NotADirectory => {
            create_sys_error(strand, global.types.not_adirectory, message, system_code)
        }
        ErrorKind::IsADirectory => {
            create_sys_error(strand, global.types.is_adirectory, message, system_code)
        }
        ErrorKind::DirectoryNotEmpty => create_sys_error(
            strand,
            global.types.directory_not_empty,
            message,
            system_code,
        ),
        ErrorKind::ReadOnlyFilesystem => create_sys_error(
            strand,
            global.types.read_only_filesystem,
            message,
            system_code,
        ),
        ErrorKind::StaleNetworkFileHandle => create_sys_error(
            strand,
            global.types.stale_network_file_handle,
            message,
            system_code,
        ),
        ErrorKind::WriteZero => {
            create_sys_error(strand, global.types.write_zero, message, system_code)
        }
        ErrorKind::StorageFull => {
            create_sys_error(strand, global.types.storage_full, message, system_code)
        }
        ErrorKind::NotSeekable => {
            create_sys_error(strand, global.types.not_seekable, message, system_code)
        }
        ErrorKind::QuotaExceeded => {
            create_sys_error(strand, global.types.quota_exceeded, message, system_code)
        }
        ErrorKind::FileTooLarge => {
            create_sys_error(strand, global.types.file_too_large, message, system_code)
        }
        ErrorKind::ResourceBusy => {
            create_sys_error(strand, global.types.resource_busy, message, system_code)
        }
        ErrorKind::ExecutableFileBusy => create_sys_error(
            strand,
            global.types.executable_file_busy,
            message,
            system_code,
        ),
        ErrorKind::Deadlock => {
            create_sys_error(strand, global.types.deadlock, message, system_code)
        }
        ErrorKind::CrossesDevices => {
            create_sys_error(strand, global.types.crosses_devices, message, system_code)
        }
        ErrorKind::TooManyLinks => {
            create_sys_error(strand, global.types.too_many_links, message, system_code)
        }
        ErrorKind::InvalidFilename => {
            create_sys_error(strand, global.types.invalid_filename, message, system_code)
        }
        ErrorKind::ArgumentListTooLong => create_sys_error(
            strand,
            global.types.argument_list_too_long,
            message,
            system_code,
        ),
        ErrorKind::InvalidData => {
            create_sys_error(strand, global.types.invalid_data, message, system_code)
        }
        ErrorKind::Interrupted => {
            create_sys_error(strand, global.types.interrupted, message, system_code)
        }
        ErrorKind::UnexpectedEof => {
            create_sys_error(strand, global.types.unexpected_eof, message, system_code)
        }
        ErrorKind::OutOfMemory => {
            create_sys_error(strand, global.types.out_of_memory, message, system_code)
        }
        _ => create_sys_error(strand, global.types.sys_error, message, system_code),
    }
}

pub trait ErrorExt {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's>;
}

impl ErrorExt for io::Error {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        io_error(strand, self)
    }
}

impl ErrorExt for VfsError {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        vfs_error(strand, self)
    }
}

pub(crate) fn io_result<'v, 's, T>(
    strand: &mut Strand<'v, 's>,
    result: std::result::Result<T, impl ErrorExt>,
) -> Result<'v, 's, T> {
    result.map_err(|error| error.into_sys(strand))
}

pub trait ResultExt<T> {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T>;
}

impl<T, E: ErrorExt> ResultExt<T> for std::result::Result<T, E> {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T> {
        self.map_err(|error| error.into_sys(strand))
    }
}

pub(crate) fn proc_status_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    status: dolang_vfs::process::ProcessStatus,
) -> Error<'v, 's> {
    let global = strand.force_state::<ErrorGlobal<'v>>();
    let operating_system = global.local.get(strand).target().os();
    Error::object_with_annex(
        strand,
        global.types.proc_error,
        ProcError,
        ProcErrorAnnex {
            name: name.to_owned(),
            status,
            operating_system,
        },
    )
}
