use dolang::{
    compile::Compiler,
    runtime::{Output, State, method, unpack, vm::Builder},
};

use crate::global::Global;

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler.prelude().import_module("sys");
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let os_sym = builder.sym("os");
    let family_sym = builder.sym("family");
    let arch_sym = builder.sym("arch");
    let logical_count_sym = builder.sym("logical_count");
    let os = builder.sym(std::env::consts::OS);
    let family = builder.sym(std::env::consts::FAMILY);
    let arch = builder.sym(std::env::consts::ARCH);
    let logical_count = std::thread::available_parallelism().map_or(1, |count| count.get());

    builder
        .module("sys")
        .function("os_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand
                .with_slots(async move |strand, [mut std_mod, mut record]| {
                    strand.import("std", &mut std_mod).await?;
                    let record_sym = global.syms.record;
                    method!(
                        strand, std_mod, record_sym, &mut record,
                        os_sym: os,
                        family_sym: family,
                    )
                    .await?;
                    Output::set(strand, out, record);
                    Ok(())
                })
                .await
        })
        .function("cpu_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand
                .with_slots(async move |strand, [mut std_mod, mut record]| {
                    strand.import("std", &mut std_mod).await?;
                    let record_sym = global.syms.record;
                    method!(
                        strand, std_mod, record_sym, &mut record,
                        arch_sym: arch,
                        logical_count_sym: logical_count,
                    )
                    .await?;
                    Output::set(strand, out, record);
                    Ok(())
                })
                .await
        })
        .value("Error", global.types.sys_error)
        .value("NotFoundError", global.types.not_found)
        .value("PermissionDeniedError", global.types.permission_denied)
        .value("AlreadyExistsError", global.types.already_exists)
        .value("TimedOutError", global.types.timed_out)
        .commit();
}
