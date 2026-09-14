use dolang::{
    compile::Config,
    runtime::{Error, Object, Output, State, Sym, object::TypeBuilder, unpack, vm::Register},
};
use dolang_vfs::target::{Architecture, OperatingSystem, OperatingSystemFamily};

use crate::global::{ErrorGlobal, SysGlobal};

pub(crate) fn configure_compiler<'a>(_config: &mut Config<'a>) {}

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

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    global: State<'v, SysGlobal<'v>>,
    errors: State<'v, ErrorGlobal<'v>>,
) {
    let linux = builder.sym("LINUX");
    let macos = builder.sym("MACOS");
    let freebsd = builder.sym("FREEBSD");
    let windows = builder.sym("WINDOWS");
    let unix = builder.sym("UNIX");
    let x86_64 = builder.sym("X86_64");
    let aarch64 = builder.sym("AARCH64");

    builder
        .module("sys")
        .function("os_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let target = global.local.get(strand).target();
            let os = match target.os() {
                OperatingSystem::FreeBsd => freebsd,
                OperatingSystem::Linux => linux,
                OperatingSystem::Macos => macos,
                OperatingSystem::Windows => windows,
                _ => return Err(Error::not_supported(strand)),
            };
            let family = match target.os().family() {
                OperatingSystemFamily::Unix => unix,
                OperatingSystemFamily::Windows => windows,
            };
            global.types.os_info.create_with_annex(
                strand,
                OsInfo,
                OsInfoAnnex {
                    os,
                    family,
                    is_wine: target.is_wine(),
                },
                out,
            );
            Ok(())
        })
        .function("cpu_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let target = global.local.get(strand).target();
            let arch = match target.arch() {
                Architecture::X86_64 => x86_64,
                Architecture::Aarch64 => aarch64,
                _ => return Err(Error::not_supported(strand)),
            };
            global.types.cpu_info.create_with_annex(
                strand,
                CpuInfo,
                CpuInfoAnnex {
                    arch,
                    logical_count: target.logical_cpus(),
                },
                out,
            );
            Ok(())
        })
        .value("OsInfo", global.types.os_info)
        .value("CpuInfo", global.types.cpu_info)
        .value("ErrorCode", errors.types.error_code)
        .value("Error", errors.types.sys_error)
        .value("InvalidInputError", errors.types.invalid_input)
        .value("NotFoundError", errors.types.not_found)
        .value("PermissionDeniedError", errors.types.permission_denied)
        .value("AlreadyExistsError", errors.types.already_exists)
        .value("TimedOutError", errors.types.timed_out)
        .value("UnsupportedError", errors.types.unsupported)
        .value("ConnectionRefusedError", errors.types.connection_refused)
        .value("ConnectionResetError", errors.types.connection_reset)
        .value("HostUnreachableError", errors.types.host_unreachable)
        .value("NetworkUnreachableError", errors.types.network_unreachable)
        .value("ConnectionAbortedError", errors.types.connection_aborted)
        .value("NotConnectedError", errors.types.not_connected)
        .value("AddrInUseError", errors.types.addr_in_use)
        .value("AddrNotAvailableError", errors.types.addr_not_available)
        .value("NetworkDownError", errors.types.network_down)
        .value("BrokenPipeError", errors.types.broken_pipe)
        .value("WouldBlockError", errors.types.would_block)
        .value("NotADirectoryError", errors.types.not_adirectory)
        .value("IsADirectoryError", errors.types.is_adirectory)
        .value("DirectoryNotEmptyError", errors.types.directory_not_empty)
        .value("ReadOnlyFilesystemError", errors.types.read_only_filesystem)
        .value(
            "StaleNetworkFileHandleError",
            errors.types.stale_network_file_handle,
        )
        .value("WriteZeroError", errors.types.write_zero)
        .value("StorageFullError", errors.types.storage_full)
        .value("NotSeekableError", errors.types.not_seekable)
        .value("QuotaExceededError", errors.types.quota_exceeded)
        .value("FileTooLargeError", errors.types.file_too_large)
        .value("ResourceBusyError", errors.types.resource_busy)
        .value("ExecutableFileBusyError", errors.types.executable_file_busy)
        .value("DeadlockError", errors.types.deadlock)
        .value("CrossesDevicesError", errors.types.crosses_devices)
        .value("TooManyLinksError", errors.types.too_many_links)
        .value("InvalidFilenameError", errors.types.invalid_filename)
        .value(
            "ArgumentListTooLongError",
            errors.types.argument_list_too_long,
        )
        .value("InvalidDataError", errors.types.invalid_data)
        .value("InterruptedError", errors.types.interrupted)
        .value("UnexpectedEofError", errors.types.unexpected_eof)
        .value("OutOfMemoryError", errors.types.out_of_memory)
        .commit();
}
