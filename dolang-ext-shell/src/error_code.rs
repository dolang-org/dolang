use std::marker::PhantomData;

use dolang::runtime::object::fmt;
use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value, object::TypeBuilder,
    unpack,
};
use dolang_vfs::OperatingSystem;

use crate::global::Global;

macro_rules! error_codes {
    (
        $by_code:ident,
        $by_name:ident,
        $ty:ty,
        { $($value:literal => $name:literal,)* },
        { $($alias:literal => $alias_value:literal,)* }
    ) => {
        pub(crate) static $by_code: phf::Map<$ty, &'static str> = phf::phf_map! {
            $($value => $name,)*
        };
        pub(crate) static $by_name: phf::Map<&'static str, $ty> = phf::phf_map! {
            $($name => $value,)*
            $($alias => $alias_value,)*
        };
    };
}

mod generated;

pub(crate) struct ErrorCode;
pub(crate) struct Errno;
pub(crate) struct FreeBsdErrno;
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
    const OPERATING_SYSTEM: Option<OperatingSystem> = None;

    fn value(raw: i32) -> i64 {
        i64::from(raw)
    }

    fn name(_value: i64) -> Option<&'static str> {
        None
    }

    fn code(_name: &str) -> Option<i32> {
        None
    }

    fn raw(value: i128) -> Option<i32> {
        i32::try_from(value).ok()
    }
}

pub(crate) fn system_code_name(
    operating_system: OperatingSystem,
    raw: i32,
) -> Option<&'static str> {
    match operating_system {
        OperatingSystem::FreeBsd => generated::FREEBSD_ERRNO_BY_CODE.get(&raw).copied(),
        OperatingSystem::Linux => generated::LINUX_ERRNO_BY_CODE.get(&raw).copied(),
        OperatingSystem::Macos => generated::MACOS_ERRNO_BY_CODE.get(&raw).copied(),
        OperatingSystem::Windows => dolang_winterop::win_error_name(raw as u32),
    }
}

impl<'v, T: CodeType<'v>> Object<'v> for CodeObject<T> {
    const NAME: &'v str = T::NAME;
    const MODULE: &'v str = T::MODULE;
    type Annex = CodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let value = value
            .as_int(strand)
            .and_then(T::raw)
            .ok_or_else(|| Error::value(strand, "system error code is out of range"))?;
        let operating_system = T::OPERATING_SYSTEM.ok_or_else(|| {
            Error::type_error(
                strand,
                format!("{}.{} is not instantiable", T::MODULE, T::NAME),
            )
        })?;
        create(strand, this, operating_system, value, out);
        Ok(())
    }

    fn type_get<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: dolang::runtime::Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let raw = T::code(field.as_str(strand)).ok_or_else(|| Error::field(strand, field))?;
        let operating_system = T::OPERATING_SYSTEM.expect("code lookup on abstract code type");
        create(strand, this, operating_system, raw, out);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder.get("value", |this, strand, out| {
            Output::set(strand, out, this.annex().value);
            Ok(())
        });
        if T::ERRNO {
            let linux = builder.sym("LINUX");
            let macos = builder.sym("MACOS");
            let freebsd = builder.sym("FREEBSD");
            builder = builder.get("os", move |this, strand, out| {
                let os = match this.annex().operating_system {
                    Some(OperatingSystem::FreeBsd) => freebsd,
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

impl<'v> CodeType<'v> for FreeBsdErrno {
    const NAME: &'v str = "Errno";
    const MODULE: &'v str = "sys.freebsd";
    const ERRNO: bool = true;
    const OPERATING_SYSTEM: Option<OperatingSystem> = Some(OperatingSystem::FreeBsd);

    fn name(value: i64) -> Option<&'static str> {
        i32::try_from(value)
            .ok()
            .and_then(|value| generated::FREEBSD_ERRNO_BY_CODE.get(&value).copied())
    }

    fn code(name: &str) -> Option<i32> {
        generated::FREEBSD_ERRNO_BY_NAME.get(name).copied()
    }
}

impl<'v> CodeType<'v> for LinuxErrno {
    const NAME: &'v str = "Errno";
    const MODULE: &'v str = "sys.linux";
    const ERRNO: bool = true;
    const OPERATING_SYSTEM: Option<OperatingSystem> = Some(OperatingSystem::Linux);

    fn name(value: i64) -> Option<&'static str> {
        i32::try_from(value)
            .ok()
            .and_then(|value| generated::LINUX_ERRNO_BY_CODE.get(&value).copied())
    }

    fn code(name: &str) -> Option<i32> {
        generated::LINUX_ERRNO_BY_NAME.get(name).copied()
    }
}

impl<'v> CodeType<'v> for MacosErrno {
    const NAME: &'v str = "Errno";
    const MODULE: &'v str = "sys.macos";
    const ERRNO: bool = true;
    const OPERATING_SYSTEM: Option<OperatingSystem> = Some(OperatingSystem::Macos);

    fn name(value: i64) -> Option<&'static str> {
        i32::try_from(value)
            .ok()
            .and_then(|value| generated::MACOS_ERRNO_BY_CODE.get(&value).copied())
    }

    fn code(name: &str) -> Option<i32> {
        generated::MACOS_ERRNO_BY_NAME.get(name).copied()
    }
}

impl<'v> CodeType<'v> for WinError {
    const NAME: &'v str = "WinError";
    const MODULE: &'v str = "sys.windows";
    const OPERATING_SYSTEM: Option<OperatingSystem> = Some(OperatingSystem::Windows);

    fn value(raw: i32) -> i64 {
        i64::from(raw as u32)
    }

    fn name(value: i64) -> Option<&'static str> {
        u32::try_from(value)
            .ok()
            .and_then(dolang_winterop::win_error_name)
    }

    fn code(name: &str) -> Option<i32> {
        dolang_winterop::win_error_code(name).map(|value| value as i32)
    }

    fn raw(value: i128) -> Option<i32> {
        u32::try_from(value).ok().map(|value| value as i32)
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
        OperatingSystem::FreeBsd => create(
            strand,
            global.types.freebsd_errno,
            operating_system,
            raw,
            out,
        ),
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

pub(crate) fn extract_system_code<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<(OperatingSystem, i32)> {
    let global = strand.state::<Global<'v>>();
    if let Some(value) = global.types.freebsd_errno.cast(value) {
        return value.enter_sync(strand, |_strand, value| {
            Some((OperatingSystem::FreeBsd, value.annex().value as i32))
        });
    }
    if let Some(value) = global.types.linux_errno.cast(value) {
        return value.enter_sync(strand, |_strand, value| {
            Some((OperatingSystem::Linux, value.annex().value as i32))
        });
    }
    if let Some(value) = global.types.macos_errno.cast(value) {
        return value.enter_sync(strand, |_strand, value| {
            Some((OperatingSystem::Macos, value.annex().value as i32))
        });
    }
    if let Some(value) = global.types.win_error.cast(value) {
        return value.enter_sync(strand, |_strand, value| {
            Some((OperatingSystem::Windows, value.annex().value as u32 as i32))
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{CodeType, FreeBsdErrno, LinuxErrno, MacosErrno, WinError};

    #[test]
    fn known_names_are_platform_specific() {
        assert_eq!(LinuxErrno::name(2), Some("ENOENT"));
        assert_eq!(LinuxErrno::name(11), Some("EAGAIN"));
        assert_eq!(FreeBsdErrno::name(2), Some("ENOENT"));
        assert_eq!(FreeBsdErrno::name(35), Some("EAGAIN"));
        assert_eq!(FreeBsdErrno::name(93), Some("ENOTCAPABLE"));
        assert_eq!(MacosErrno::name(2), Some("ENOENT"));
        assert_eq!(MacosErrno::name(35), Some("EAGAIN"));
        assert_eq!(WinError::name(2), Some("ERROR_FILE_NOT_FOUND"));
    }

    #[test]
    fn unknown_names_use_no_lookup_result() {
        assert_eq!(LinuxErrno::name(i64::MAX), None);
        assert_eq!(FreeBsdErrno::name(i64::MAX), None);
        assert_eq!(MacosErrno::name(i64::MAX), None);
        assert_eq!(WinError::name(i64::MAX), None);
    }

    #[test]
    fn winerror_value_preserves_unsigned_dword_bits() {
        assert_eq!(WinError::value(-1), i64::from(u32::MAX));
    }

    #[test]
    fn errno_aliases_support_reverse_lookup() {
        assert_eq!(LinuxErrno::code("EWOULDBLOCK"), Some(11));
        assert_eq!(FreeBsdErrno::code("EWOULDBLOCK"), Some(35));
        assert_eq!(MacosErrno::code("EWOULDBLOCK"), Some(35));
    }
}
