extern crate dolang_ext_shell;

use std::{env, path::Path};

use dolang::{compile::Config, extension::CompilerExt};
use dolang_private_bundle::{Bundle, CompileMode, NameMode};

/// The entrypoints run under the shell's prelude.
fn configure(config: &mut Config<'_>) {
    for ext in config.extensions() {
        ext.apply(config).unwrap();
    }
}

fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var_os("OUT_DIR").unwrap();

    dolang_private_bundle::bundle(&Bundle {
        source_root: &Path::new(&manifest_dir).join("entrypoint"),
        out_dir: Path::new(&out_dir),
        virtual_root: "<entrypoint>",
        compile_mode: CompileMode::Script,
        name_mode: NameMode::Stem,
        configure,
        bytecode: Some("bundled_entrypoints"),
        typelibs: None,
    });
}
