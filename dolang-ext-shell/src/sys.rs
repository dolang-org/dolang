use dolang::{
    compile::Compiler,
    runtime::{Output, State, method, unpack, vm::Builder},
};

use crate::global::Global;

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler.prelude().import_module("sys");
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let kind_sym = builder.sym("kind");
    let archetype_sym = builder.sym("archetype");
    let os_kind = builder.sym(std::env::consts::OS);
    let os_archetype = if std::env::consts::OS == "windows" {
        builder.sym("windows")
    } else {
        builder.sym("unix")
    };

    builder
        .module("sys")
        .function("os", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand
                .with_slots(async move |strand, [mut std_mod, mut record]| {
                    strand.import("std", &mut std_mod).await?;
                    let record_sym = global.syms.record;
                    method!(
                        strand, std_mod, record_sym, &mut record,
                        kind_sym: os_kind,
                        archetype_sym: os_archetype,
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
