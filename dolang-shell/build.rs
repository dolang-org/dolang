use std::{env, path::Path};

use dolang_private_bundle::{Bundle, CompileMode, NameMode};

fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var_os("OUT_DIR").unwrap();

    dolang_private_bundle::bundle(&Bundle {
        source_root: &Path::new(&manifest_dir).join("entrypoint"),
        out_dir: Path::new(&out_dir),
        output_name: "bundled_entrypoints",
        table_name: "BUNDLED_ENTRYPOINTS",
        virtual_root: "<entrypoint>",
        compile_mode: CompileMode::Script,
        name_mode: NameMode::Stem,
    });
}
