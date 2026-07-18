use std::marker::PhantomData;

use dolang::runtime::object::fmt;
use dolang::runtime::{Instance, Object, Output, Result, Strand, Type, object::TypeBuilder};
use dolang_shell_vfs::OperatingSystem;

use crate::global::Global;

mod generated;

pub(crate) struct ErrorCode;
pub(crate) struct Errno;
pub(crate) struct LinuxErrno;
pub(crate) struct MacosErrno;
pub(crate) struct WinError;

pub(crate) struct CodeObject<T>(PhantomData<T>);

impl<T> Default for CodeObject<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct CodeAnnex {
    value: i64,
    operating_system: Option<OperatingSystem>,
}

trait CodeType<'v>: Sized + 'v {
    const NAME: &'v str;
    const MODULE: &'v str;
    const ERRNO: bool = false;

    fn value(raw: i32) -> i64 {
        i64::from(raw)
    }

    fn name(_value: i64) -> Option<&'static str> {
        None
    }
}

fn lookup<T: Copy + Ord>(table: &'static [(T, &'static str)], value: T) -> Option<&'static str> {
    table
        .binary_search_by_key(&value, |(value, _)| *value)
        .ok()
        .map(|index| table[index].1)
}

pub(crate) fn system_code_name(
    operating_system: OperatingSystem,
    raw: i32,
) -> Option<&'static str> {
    match operating_system {
        OperatingSystem::Linux => lookup(generated::LINUX_ERRNO, raw),
        OperatingSystem::Macos => lookup(generated::MACOS_ERRNO, raw),
        OperatingSystem::Windows => lookup(generated::WIN_ERROR, raw as u32),
    }
}

impl<'v, T: CodeType<'v>> Object<'v> for CodeObject<T> {
    const NAME: &'v str = T::NAME;
    const MODULE: &'v str = T::MODULE;
    type Annex = CodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder.get("value", |this, strand, out| {
            Output::set(strand, out, this.annex().value);
            Ok(())
        });
        if T::ERRNO {
            let linux = builder.sym("LINUX");
            let macos = builder.sym("MACOS");
            builder = builder.get("os", move |this, strand, out| {
                let os = match this.annex().operating_system {
                    Some(OperatingSystem::Linux) => linux,
                    Some(OperatingSystem::Macos) => macos,
                    Some(OperatingSystem::Windows) | None => unreachable!("invalid errno OS"),
                };
                Output::set(strand, out, os);
                Ok(())
            });
        }
        builder
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        if let Some(name) = T::name(this.annex().value) {
            fmt!(strand, w, "{}", name)
        } else {
            fmt!(strand, w, "{}", this.annex().value)
        }
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<{}.{} ", T::MODULE, T::NAME)?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> CodeType<'v> for ErrorCode {
    const NAME: &'v str = "ErrorCode";
    const MODULE: &'v str = "sys";
}

impl<'v> CodeType<'v> for Errno {
    const NAME: &'v str = "Errno";
    const MODULE: &'v str = "sys.unix";
    const ERRNO: bool = true;
}

impl<'v> CodeType<'v> for LinuxErrno {
    const NAME: &'v str = "LinuxErrno";
    const MODULE: &'v str = "sys.linux";
    const ERRNO: bool = true;

    fn name(value: i64) -> Option<&'static str> {
        i32::try_from(value)
            .ok()
            .and_then(|value| lookup(generated::LINUX_ERRNO, value))
    }
}

impl<'v> CodeType<'v> for MacosErrno {
    const NAME: &'v str = "MacosErrno";
    const MODULE: &'v str = "sys.macos";
    const ERRNO: bool = true;

    fn name(value: i64) -> Option<&'static str> {
        i32::try_from(value)
            .ok()
            .and_then(|value| lookup(generated::MACOS_ERRNO, value))
    }
}

impl<'v> CodeType<'v> for WinError {
    const NAME: &'v str = "WinError";
    const MODULE: &'v str = "sys.windows";

    fn value(raw: i32) -> i64 {
        i64::from(raw as u32)
    }

    fn name(value: i64) -> Option<&'static str> {
        u32::try_from(value)
            .ok()
            .and_then(|value| lookup(generated::WIN_ERROR, value))
    }
}

fn create<'v, 's, T: CodeType<'v>>(
    strand: &mut Strand<'v, 's>,
    ty: Type<'v, CodeObject<T>>,
    operating_system: OperatingSystem,
    raw: i32,
    out: impl Output<'v>,
) {
    ty.create_with_annex(
        strand,
        CodeObject::default(),
        CodeAnnex {
            value: T::value(raw),
            operating_system: T::ERRNO.then_some(operating_system),
        },
        out,
    );
}

pub(crate) fn create_system_code<'v, 's>(
    strand: &mut Strand<'v, 's>,
    operating_system: OperatingSystem,
    raw: i32,
    out: impl Output<'v>,
) {
    let global = strand.state::<Global<'v>>();
    match operating_system {
        OperatingSystem::Linux => {
            create(strand, global.types.linux_errno, operating_system, raw, out)
        }
        OperatingSystem::Macos => {
            create(strand, global.types.macos_errno, operating_system, raw, out)
        }
        OperatingSystem::Windows => {
            create(strand, global.types.win_error, operating_system, raw, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CodeType, LinuxErrno, MacosErrno, WinError};

    #[test]
    fn known_names_are_platform_specific() {
        assert_eq!(LinuxErrno::name(2), Some("ENOENT"));
        assert_eq!(LinuxErrno::name(11), Some("EAGAIN"));
        assert_eq!(MacosErrno::name(2), Some("ENOENT"));
        assert_eq!(MacosErrno::name(35), Some("EAGAIN"));
        assert_eq!(WinError::name(2), Some("ERROR_FILE_NOT_FOUND"));
    }

    #[test]
    fn unknown_names_use_no_lookup_result() {
        assert_eq!(LinuxErrno::name(i64::MAX), None);
        assert_eq!(MacosErrno::name(i64::MAX), None);
        assert_eq!(WinError::name(i64::MAX), None);
    }

    #[test]
    fn winerror_value_preserves_unsigned_dword_bits() {
        assert_eq!(WinError::value(-1), i64::from(u32::MAX));
    }
}
