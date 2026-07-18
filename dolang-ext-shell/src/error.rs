use std::{fmt, io, marker::PhantomData};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Strand, Type, object::TypeBuilder, value::TypeObject,
};

use crate::{error_code, global::Global};

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
        let message = self.error.to_string();
        let Some((operating_system, code)) = self.error.system_code(self.operating_system) else {
            return message;
        };
        let Some(name) = error_code::system_code_name(operating_system, code) else {
            return message;
        };
        format!("{message} ({name})")
    }
}

impl SysErrorSource {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) => error.kind(),
            Self::Vfs(error) => error.kind(),
        }
    }

    fn system_code(
        &self,
        default_operating_system: dolang_shell_vfs::OperatingSystem,
    ) -> Option<(dolang_shell_vfs::OperatingSystem, i32)> {
        match self {
            Self::Io(error) => error
                .raw_os_error()
                .map(|code| (default_operating_system, code)),
            Self::Vfs(error) => error
                .system()
                .map(|error| (*error.operating_system(), error.code())),
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

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("code", |this, strand, out| {
            if let Some((operating_system, code)) = this
                .annex()
                .error
                .system_code(this.annex().operating_system)
            {
                error_code::create_system_code(strand, operating_system, code, out);
            }
            Ok(())
        })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<sys.{} ", T::NAME)?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
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
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<proc.Error ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
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

#[cfg(test)]
mod tests {
    use std::io;

    use dolang_shell_vfs::{OperatingSystem, SystemError};

    use super::{SysErrorAnnex, SysErrorSource};

    #[test]
    fn io_error_without_native_code_has_no_system_code() {
        let error = SysErrorSource::Io(io::Error::other("synthetic"));
        assert_eq!(error.system_code(OperatingSystem::Linux), None);
    }

    #[test]
    fn vfs_system_code_preserves_its_origin() {
        let error = SysErrorSource::Vfs(
            SystemError::new(
                OperatingSystem::Windows,
                2,
                io::ErrorKind::NotFound,
                "missing",
            )
            .into(),
        );
        assert_eq!(
            error.system_code(OperatingSystem::Linux),
            Some((OperatingSystem::Windows, 2))
        );
    }

    #[test]
    fn sys_error_message_appends_known_symbolic_code() {
        let error = SysErrorAnnex {
            error: SysErrorSource::Vfs(
                SystemError::new(
                    OperatingSystem::Linux,
                    2,
                    io::ErrorKind::NotFound,
                    "missing",
                )
                .into(),
            ),
            operating_system: OperatingSystem::Linux,
        };
        assert_eq!(error.message(), "missing (ENOENT)");
    }

    #[test]
    fn sys_error_message_leaves_unknown_code_numeric_only() {
        let error = SysErrorAnnex {
            error: SysErrorSource::Vfs(
                SystemError::new(
                    OperatingSystem::Linux,
                    i32::MAX,
                    io::ErrorKind::Other,
                    "unknown",
                )
                .into(),
            ),
            operating_system: OperatingSystem::Linux,
        };
        assert_eq!(error.message(), "unknown");
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
