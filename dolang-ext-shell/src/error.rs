use std::{fmt, io, marker::PhantomData};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Strand, Type, object::TypeBuilder, value::TypeObject,
};

use crate::global::Global;

pub(crate) struct SysError;
pub(crate) struct NotFoundError;
pub(crate) struct PermissionDeniedError;
pub(crate) struct AlreadyExistsError;
pub(crate) struct TimedOutError;
pub(crate) struct UnsupportedError;

pub(crate) struct SysErrorObject<T>(PhantomData<T>);

impl<T> Default for SysErrorObject<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct SysErrorAnnex {
    pub(crate) error: SysErrorSource,
    pub(crate) operating_system: dolang_shell_vfs::OperatingSystem,
}

pub(crate) enum SysErrorSource {
    Io(io::Error),
    Vfs(dolang_shell_vfs::Error),
}

impl SysErrorAnnex {
    fn message(&self) -> String {
        self.error.to_string()
    }
}

impl SysErrorSource {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) => error.kind(),
            Self::Vfs(error) => error.kind(),
        }
    }

    fn errno(&self) -> Option<i32> {
        match self {
            Self::Io(error) => {
                #[cfg(unix)]
                return error.raw_os_error();
                #[cfg(not(unix))]
                {
                    let _ = error;
                    None
                }
            }
            Self::Vfs(error) => error.system().and_then(|error| {
                matches!(
                    error.operating_system(),
                    dolang_shell_vfs::OperatingSystem::Linux
                        | dolang_shell_vfs::OperatingSystem::Macos
                )
                .then(|| error.code())
            }),
        }
    }

    fn winerror(&self) -> Option<i32> {
        match self {
            Self::Io(error) => {
                #[cfg(windows)]
                return error.raw_os_error();
                #[cfg(not(windows))]
                {
                    let _ = error;
                    None
                }
            }
            Self::Vfs(error) => error.system().and_then(|error| {
                matches!(
                    error.operating_system(),
                    dolang_shell_vfs::OperatingSystem::Windows
                )
                .then(|| error.code())
            }),
        }
    }
}

impl fmt::Display for SysErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Vfs(error) => error.fmt(f),
        }
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

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let errno = builder.sym("errno");
        let winerror = builder.sym("winerror");
        builder
            .get("errno", move |this, strand, out| {
                if matches!(
                    this.annex().operating_system,
                    dolang_shell_vfs::OperatingSystem::Windows
                ) {
                    return Err(Error::field(strand, errno));
                }
                if let Some(errno) = this.annex().error.errno() {
                    Output::set(strand, out, i64::from(errno));
                }
                Ok(())
            })
            .get("winerror", move |this, strand, out| {
                if !matches!(
                    this.annex().operating_system,
                    dolang_shell_vfs::OperatingSystem::Windows
                ) {
                    return Err(Error::field(strand, winerror));
                }
                if let Some(winerror) = this.annex().error.winerror() {
                    Output::set(strand, out, i64::from(winerror));
                }
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().message()).map_err(|err| Error::runtime(strand, err))
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<sys.{} ", T::NAME).map_err(|err| Error::runtime(strand, err))?;
        Self::display(this, strand, w)?;
        write!(w, ">").map_err(|err| Error::runtime(strand, err))
    }
}

impl<'v> SysErrorType<'v> for SysError {
    const NAME: &'v str = "Error";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysErrorClass {
    Error,
    NotFoundError,
    PermissionDeniedError,
    AlreadyExistsError,
    TimedOutError,
    UnsupportedError,
}

fn classify_io_error_kind(kind: io::ErrorKind) -> SysErrorClass {
    match kind {
        io::ErrorKind::NotFound => SysErrorClass::NotFoundError,
        io::ErrorKind::PermissionDenied => SysErrorClass::PermissionDeniedError,
        io::ErrorKind::AlreadyExists => SysErrorClass::AlreadyExistsError,
        io::ErrorKind::TimedOut => SysErrorClass::TimedOutError,
        io::ErrorKind::Unsupported => SysErrorClass::UnsupportedError,
        _ => SysErrorClass::Error,
    }
}

fn create_sys_error<'v, 's, T: SysErrorType<'v>>(
    strand: &mut Strand<'v, 's>,
    ty: Type<'v, SysErrorObject<T>>,
    error: SysErrorSource,
    operating_system: dolang_shell_vfs::OperatingSystem,
) -> Error<'v, 's> {
    Error::object_with_annex(
        strand,
        ty,
        SysErrorObject::<T>::default(),
        SysErrorAnnex {
            error,
            operating_system,
        },
    )
}

pub(crate) struct ProcError;

pub(crate) struct ProcErrorAnnex {
    pub(crate) name: String,
    pub(crate) status: dolang_shell_vfs::ProcessStatus,
    pub(crate) operating_system: dolang_shell_vfs::OperatingSystem,
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
            if matches!(
                this.annex().operating_system,
                dolang_shell_vfs::OperatingSystem::Windows
            ) {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().message()).map_err(|err| Error::runtime(strand, err))
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<proc.Error ").map_err(|err| Error::runtime(strand, err))?;
        Self::display(this, strand, w)?;
        write!(w, ">").map_err(|err| Error::runtime(strand, err))
    }
}

pub(crate) fn io_error<'v, 's>(strand: &mut Strand<'v, 's>, error: io::Error) -> Error<'v, 's> {
    sys_error(strand, SysErrorSource::Io(error))
}

pub(crate) fn vfs_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: dolang_shell_vfs::Error,
) -> Error<'v, 's> {
    sys_error(strand, SysErrorSource::Vfs(error))
}

fn sys_error<'v, 's>(strand: &mut Strand<'v, 's>, error: SysErrorSource) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    let operating_system = global.local.get(strand).target().operating_system;
    match classify_io_error_kind(error.kind()) {
        SysErrorClass::Error => {
            create_sys_error::<SysError>(strand, global.types.sys_error, error, operating_system)
        }
        SysErrorClass::NotFoundError => create_sys_error::<NotFoundError>(
            strand,
            global.types.not_found,
            error,
            operating_system,
        ),
        SysErrorClass::PermissionDeniedError => create_sys_error::<PermissionDeniedError>(
            strand,
            global.types.permission_denied,
            error,
            operating_system,
        ),
        SysErrorClass::AlreadyExistsError => create_sys_error::<AlreadyExistsError>(
            strand,
            global.types.already_exists,
            error,
            operating_system,
        ),
        SysErrorClass::TimedOutError => create_sys_error::<TimedOutError>(
            strand,
            global.types.timed_out,
            error,
            operating_system,
        ),
        SysErrorClass::UnsupportedError => create_sys_error::<UnsupportedError>(
            strand,
            global.types.unsupported,
            error,
            operating_system,
        ),
    }
}

pub(crate) trait ErrorExt {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's>;
}

impl ErrorExt for io::Error {
    fn into_sys<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        io_error(strand, self)
    }
}

impl ErrorExt for dolang_shell_vfs::Error {
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

pub(crate) trait ResultExt<T> {
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
    status: dolang_shell_vfs::ProcessStatus,
) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    let operating_system = global.local.get(strand).target().operating_system;
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

#[cfg(test)]
mod test {
    use super::{SysErrorClass, classify_io_error_kind};
    use std::io::ErrorKind;

    #[test]
    fn classify_common_io_kinds() {
        assert_eq!(
            classify_io_error_kind(ErrorKind::NotFound),
            SysErrorClass::NotFoundError
        );
        assert_eq!(
            classify_io_error_kind(ErrorKind::PermissionDenied),
            SysErrorClass::PermissionDeniedError
        );
        assert_eq!(
            classify_io_error_kind(ErrorKind::AlreadyExists),
            SysErrorClass::AlreadyExistsError
        );
        assert_eq!(
            classify_io_error_kind(ErrorKind::TimedOut),
            SysErrorClass::TimedOutError
        );
        assert_eq!(
            classify_io_error_kind(ErrorKind::Unsupported),
            SysErrorClass::UnsupportedError
        );
        assert_eq!(
            classify_io_error_kind(ErrorKind::InvalidInput),
            SysErrorClass::Error
        );
    }
}
