#![deny(warnings)]

mod detail {
    use dolang::runtime::{Error, vm::Builder};
    use std::path::{Path, PathBuf};
    extern crate dolang_ext_xml;

    const MOD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mod");

    pub(super) fn run(path: &Path) {
        futures::executor::block_on(async {
            Builder::build(async |vm| {
                let test_state = dolang_private_test::configure_vm(vm);
                dolang_private_test::apply_vm_extensions(vm);
                vm.importer(async move |strand, name, out| {
                    let path: PathBuf = [Path::new(MOD_DIR), Path::new(&format!("{}.dol", name))]
                        .into_iter()
                        .collect();
                    if !path.exists() {
                        return Err(Error::import(strand, name));
                    }
                    let (_, bytecode, _, _) =
                        dolang_private_test::compile_standard(&path, Some(name));
                    bytecode.unwrap().run(strand, out).await
                });
                vm.enter_with_slots(async move |strand, [retval]| {
                    let (content, bytecode, diags, directives) =
                        dolang_private_test::compile_standard(path, None);
                    dolang_private_test::vm_run(
                        strand,
                        path,
                        &content,
                        bytecode,
                        diags,
                        directives,
                        &test_state,
                        retval,
                    )
                    .await;
                })
                .await;
            })
            .await
        })
    }
}

use detail::run;

include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));
