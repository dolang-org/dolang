use std::env::consts;

use dolang::{
    compile::Compiler,
    runtime::{Object, Output, State, Sym, object::TypeBuilder, unpack, vm::Builder},
};

use crate::global::Global;

#[cfg(windows)]
fn is_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::core::w;

    const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";

    let ntdll = unsafe { GetModuleHandleW(w!("ntdll.dll")) };
    !ntdll.is_null() && unsafe { GetProcAddress(ntdll, WINE_GET_VERSION.as_ptr()) }.is_some()
}

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler.prelude().import_module("sys");
}

pub(crate) struct OsInfo;

pub(crate) struct OsInfoAnnex<'v> {
    os: Sym<'v, 'v>,
    family: Sym<'v, 'v>,
    #[cfg(windows)]
    is_wine: bool,
}

impl<'v> Object<'v> for OsInfo {
    const NAME: &'v str = "OsInfo";
    const MODULE: &'v str = "sys";
    type Annex = OsInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder
            .get("os", |this, strand, out| {
                Output::set(strand, out, this.annex().os);
                Ok(())
            })
            .get("family", |this, strand, out| {
                Output::set(strand, out, this.annex().family);
                Ok(())
            });

        #[cfg(windows)]
        let builder = builder.get("is_wine", |this, strand, out| {
            Output::set(strand, out, this.annex().is_wine);
            Ok(())
        });

        builder
    }
}

pub(crate) struct CpuInfo;

pub(crate) struct CpuInfoAnnex<'v> {
    arch: Sym<'v, 'v>,
    logical_count: usize,
}

impl<'v> Object<'v> for CpuInfo {
    const NAME: &'v str = "CpuInfo";
    const MODULE: &'v str = "sys";
    type Annex = CpuInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("arch", |this, strand, out| {
                Output::set(strand, out, this.annex().arch);
                Ok(())
            })
            .get("logical_count", |this, strand, out| {
                Output::set(strand, out, this.annex().logical_count);
                Ok(())
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let os = builder.sym(&consts::OS.to_ascii_uppercase());
    let family = builder.sym(&consts::FAMILY.to_ascii_uppercase());
    #[cfg(windows)]
    let is_wine = is_wine();
    let arch = builder.sym(&consts::ARCH.to_ascii_uppercase());
    let logical_count = std::thread::available_parallelism().map_or(1, |count| count.get());

    builder
        .module("sys")
        .function("os_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            global.types.os_info.create_with_annex(
                strand,
                OsInfo,
                OsInfoAnnex {
                    os,
                    family,
                    #[cfg(windows)]
                    is_wine,
                },
                out,
            );
            Ok(())
        })
        .function("cpu_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            global.types.cpu_info.create_with_annex(
                strand,
                CpuInfo,
                CpuInfoAnnex {
                    arch,
                    logical_count,
                },
                out,
            );
            Ok(())
        })
        .value("OsInfo", global.types.os_info)
        .value("CpuInfo", global.types.cpu_info)
        .value("Error", global.types.sys_error)
        .value("NotFoundError", global.types.not_found)
        .value("PermissionDeniedError", global.types.permission_denied)
        .value("AlreadyExistsError", global.types.already_exists)
        .value("TimedOutError", global.types.timed_out)
        .commit();
}
