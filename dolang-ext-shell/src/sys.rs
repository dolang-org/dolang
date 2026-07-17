use dolang::{
    compile::Compiler,
    runtime::{Error, Object, Output, State, Sym, object::TypeBuilder, unpack, vm::Builder},
};
use dolang_shell_vfs::{Architecture, OperatingSystem, OperatingSystemFamily};

use crate::global::Global;

pub(crate) fn configure_compiler<'a>(_compiler: &mut Compiler<'a>) {}

pub(crate) struct OsInfo;

pub(crate) struct OsInfoAnnex<'v> {
    os: Sym<'v, 'v>,
    family: Sym<'v, 'v>,
    is_wine: Option<bool>,
}

impl<'v> Object<'v> for OsInfo {
    const NAME: &'v str = "OsInfo";
    const MODULE: &'v str = "sys";
    type Annex = OsInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let is_wine = builder.sym("is_wine");
        builder
            .get("os", |this, strand, out| {
                Output::set(strand, out, this.annex().os);
                Ok(())
            })
            .get("family", |this, strand, out| {
                Output::set(strand, out, this.annex().family);
                Ok(())
            })
            .get("is_wine", move |this, strand, out| {
                let Some(value) = this.annex().is_wine else {
                    return Err(Error::field(strand, is_wine));
                };
                Output::set(strand, out, value);
                Ok(())
            })
    }
}

pub(crate) struct CpuInfo;

pub(crate) struct CpuInfoAnnex<'v> {
    arch: Sym<'v, 'v>,
    logical_count: u32,
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
    let linux = builder.sym("LINUX");
    let macos = builder.sym("MACOS");
    let windows = builder.sym("WINDOWS");
    let unix = builder.sym("UNIX");
    let x86_64 = builder.sym("X86_64");
    let aarch64 = builder.sym("AARCH64");

    builder
        .module("sys")
        .function("os_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let target = global.local.get(strand).target();
            let os = match &target.operating_system {
                OperatingSystem::Linux => linux,
                OperatingSystem::Macos => macos,
                OperatingSystem::Windows => windows,
            };
            let family = match target.operating_system.family() {
                OperatingSystemFamily::Unix => unix,
                OperatingSystemFamily::Windows => windows,
            };
            global.types.os_info.create_with_annex(
                strand,
                OsInfo,
                OsInfoAnnex {
                    os,
                    family,
                    is_wine: target.is_wine,
                },
                out,
            );
            Ok(())
        })
        .function("cpu_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let target = global.local.get(strand).target();
            let arch = match target.architecture {
                Architecture::X86_64 => x86_64,
                Architecture::Aarch64 => aarch64,
            };
            global.types.cpu_info.create_with_annex(
                strand,
                CpuInfo,
                CpuInfoAnnex {
                    arch,
                    logical_count: target.logical_cpu_count,
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
        .value("UnsupportedError", global.types.unsupported)
        .commit();
}
