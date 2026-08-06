use std::{io, marker::PhantomData};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, object::TypeBuilder, unpack,
    value::TypeObject,
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
    pub(crate) message: String,
    pub(crate) system_code: Option<(dolang_vfs::OperatingSystem, i32)>,
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
        let code_sym = strand.state::<Global<'v>>().syms.code;
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

fn classify_error_kind(kind: dolang_vfs::ErrorKind) -> SysErrorClass {
    match kind {
        dolang_vfs::ErrorKind::NotFound => SysErrorClass::NotFoundError,
        dolang_vfs::ErrorKind::PermissionDenied => SysErrorClass::PermissionDeniedError,
        dolang_vfs::ErrorKind::AlreadyExists => SysErrorClass::AlreadyExistsError,
        dolang_vfs::ErrorKind::TimedOut => SysErrorClass::TimedOutError,
        dolang_vfs::ErrorKind::Unsupported => SysErrorClass::UnsupportedError,
        _ => SysErrorClass::Error,
    }
}

fn create_sys_error<'v, 's, T: SysErrorType<'v>>(
    strand: &mut Strand<'v, 's>,
    ty: Type<'v, SysErrorObject<T>>,
    message: String,
    system_code: Option<(dolang_vfs::OperatingSystem, i32)>,
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
    pub(crate) status: dolang_vfs::ProcessStatus,
    pub(crate) operating_system: dolang_vfs::OperatingSystem,
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
                dolang_vfs::OperatingSystem::Windows
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
    vfs_error(strand, error.into())
}

pub(crate) fn vfs_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: dolang_vfs::Error,
) -> Error<'v, 's> {
    sys_error(strand, error)
}

fn sys_error<'v, 's>(strand: &mut Strand<'v, 's>, error: dolang_vfs::Error) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    let message = error.message().to_owned();
    let system_code = error
        .system_code()
        .map(|code| (code.operating_system(), code.raw()));
    match classify_error_kind(error.kind()) {
        SysErrorClass::Error => {
            create_sys_error::<SysError>(strand, global.types.sys_error, message, system_code)
        }
        SysErrorClass::NotFoundError => {
            create_sys_error::<NotFoundError>(strand, global.types.not_found, message, system_code)
        }
        SysErrorClass::PermissionDeniedError => create_sys_error::<PermissionDeniedError>(
            strand,
            global.types.permission_denied,
            message,
            system_code,
        ),
        SysErrorClass::AlreadyExistsError => create_sys_error::<AlreadyExistsError>(
            strand,
            global.types.already_exists,
            message,
            system_code,
        ),
        SysErrorClass::TimedOutError => {
            create_sys_error::<TimedOutError>(strand, global.types.timed_out, message, system_code)
        }
        SysErrorClass::UnsupportedError => create_sys_error::<UnsupportedError>(
            strand,
            global.types.unsupported,
            message,
            system_code,
        ),
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

impl ErrorExt for dolang_vfs::Error {
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

#[cfg(test)]
mod tests {
    use dolang_vfs::OperatingSystem;

    use super::SysErrorAnnex;

    #[test]
    fn sys_error_message_appends_known_symbolic_code() {
        let error = SysErrorAnnex {
            message: "missing".to_owned(),
            system_code: Some((OperatingSystem::Linux, 2)),
        };
        assert_eq!(error.message(), "missing (ENOENT)");
    }

    #[test]
    fn sys_error_message_leaves_unknown_code_numeric_only() {
        let error = SysErrorAnnex {
            message: "unknown".to_owned(),
            system_code: Some((OperatingSystem::Linux, i32::MAX)),
        };
        assert_eq!(error.message(), "unknown");
    }
}

pub(crate) fn proc_status_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    status: dolang_vfs::ProcessStatus,
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
    use super::{SysErrorClass, classify_error_kind};
    use dolang_vfs::ErrorKind;

    #[test]
    fn classify_common_io_kinds() {
        assert_eq!(
            classify_error_kind(ErrorKind::NotFound),
            SysErrorClass::NotFoundError
        );
        assert_eq!(
            classify_error_kind(ErrorKind::PermissionDenied),
            SysErrorClass::PermissionDeniedError
        );
        assert_eq!(
            classify_error_kind(ErrorKind::AlreadyExists),
            SysErrorClass::AlreadyExistsError
        );
        assert_eq!(
            classify_error_kind(ErrorKind::TimedOut),
            SysErrorClass::TimedOutError
        );
        assert_eq!(
            classify_error_kind(ErrorKind::Unsupported),
            SysErrorClass::UnsupportedError
        );
        assert_eq!(
            classify_error_kind(ErrorKind::InvalidInput),
            SysErrorClass::Error
        );
    }
}
