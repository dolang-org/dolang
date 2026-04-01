use std::{fmt, io, marker::PhantomData, process::ExitStatus};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Strand, Type, object::TypeBuilder, value::TypeObject,
};

use crate::global::Global;

pub(crate) struct SysError;
pub(crate) struct NotFoundError;
pub(crate) struct PermissionDeniedError;
pub(crate) struct AlreadyExistsError;
pub(crate) struct TimedOutError;

pub(crate) struct SysErrorObject<T>(PhantomData<T>);

impl<T> Default for SysErrorObject<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct SysErrorAnnex {
    pub(crate) error: io::Error,
}

impl SysErrorAnnex {
    fn message(&self) -> String {
        self.error.to_string()
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
        #[cfg(unix)]
        let builder = builder.get("errno", |this, strand, out| {
            if let Some(errno) = this.annex().error.raw_os_error() {
                Output::set(strand, out, i64::from(errno));
            }
            Ok(())
        });
        builder
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysErrorClass {
    Error,
    NotFoundError,
    PermissionDeniedError,
    AlreadyExistsError,
    TimedOutError,
}

fn classify_io_error_kind(kind: io::ErrorKind) -> SysErrorClass {
    match kind {
        io::ErrorKind::NotFound => SysErrorClass::NotFoundError,
        io::ErrorKind::PermissionDenied => SysErrorClass::PermissionDeniedError,
        io::ErrorKind::AlreadyExists => SysErrorClass::AlreadyExistsError,
        io::ErrorKind::TimedOut => SysErrorClass::TimedOutError,
        _ => SysErrorClass::Error,
    }
}

fn create_sys_error<'v, 's, T: SysErrorType<'v>>(
    strand: &Strand<'v, 's>,
    ty: Type<'v, SysErrorObject<T>>,
    error: io::Error,
) -> Error<'v, 's> {
    Error::object_with_annex(
        strand,
        ty,
        SysErrorObject::<T>::default(),
        SysErrorAnnex { error },
    )
}

pub(crate) struct ProcError;

pub(crate) struct ProcErrorAnnex {
    pub(crate) name: String,
    pub(crate) status: ExitStatus,
}

impl ProcErrorAnnex {
    fn message(&self) -> String {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = self.status.signal() {
                return format!("{} terminated by signal: {}", self.name, signal);
            }
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
        let builder =
            builder
                .nominal_supertype(TypeObject::RuntimeError)
                .get("rc", |this, strand, out| {
                    if let Some(code) = this.annex().status.code() {
                        Output::set(strand, out, i64::from(code));
                    }
                    Ok(())
                });
        #[cfg(unix)]
        let builder = builder.get("signal", |this, strand, out| {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = this.annex().status.signal() {
                Output::set(strand, out, i64::from(signal));
            }
            Ok(())
        });
        builder
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

pub(crate) fn io_error<'v, 's>(strand: &Strand<'v, 's>, error: io::Error) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    match classify_io_error_kind(error.kind()) {
        SysErrorClass::Error => create_sys_error::<SysError>(strand, global.types.sys_error, error),
        SysErrorClass::NotFoundError => {
            create_sys_error::<NotFoundError>(strand, global.types.not_found, error)
        }
        SysErrorClass::PermissionDeniedError => {
            create_sys_error::<PermissionDeniedError>(strand, global.types.permission_denied, error)
        }
        SysErrorClass::AlreadyExistsError => {
            create_sys_error::<AlreadyExistsError>(strand, global.types.already_exists, error)
        }
        SysErrorClass::TimedOutError => {
            create_sys_error::<TimedOutError>(strand, global.types.timed_out, error)
        }
    }
}

pub(crate) trait ErrorExt {
    fn into_sys<'v, 's>(self, strand: &Strand<'v, 's>) -> Error<'v, 's>;
}

impl ErrorExt for io::Error {
    fn into_sys<'v, 's>(self, strand: &Strand<'v, 's>) -> Error<'v, 's> {
        io_error(strand, self)
    }
}

pub(crate) fn io_result<'v, 's, T>(
    strand: &Strand<'v, 's>,
    result: io::Result<T>,
) -> Result<'v, 's, T> {
    result.map_err(|error| io_error(strand, error))
}

pub(crate) trait ResultExt<T> {
    fn into_sys<'v, 's>(self, strand: &Strand<'v, 's>) -> Result<'v, 's, T>;
}

impl<T> ResultExt<T> for io::Result<T> {
    fn into_sys<'v, 's>(self, strand: &Strand<'v, 's>) -> Result<'v, 's, T> {
        io_result(strand, self)
    }
}

pub(crate) fn proc_status_error<'v, 's>(
    strand: &Strand<'v, 's>,
    name: &str,
    status: ExitStatus,
) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    Error::object_with_annex(
        strand,
        global.types.proc_error,
        ProcError,
        ProcErrorAnnex {
            name: name.to_owned(),
            status,
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
            classify_io_error_kind(ErrorKind::InvalidInput),
            SysErrorClass::Error
        );
    }
}
