use std::{env, path::Path};

use dolang_private_bundle::{Bundle, CompileMode, NameMode};

/// Bundles the typelib of the `test` shim's stub, kept out of `stub/` so the
/// docs and the LSP don't take it for a second `test` module.
fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var_os("OUT_DIR").unwrap();

    dolang_private_bundle::bundle(&Bundle {
        source_root: &Path::new(&manifest_dir).join("typelib"),
        out_dir: Path::new(&out_dir),
        virtual_root: "dolang-wasm/typelib",
        compile_mode: CompileMode::Module,
        name_mode: NameMode::Module,
        configure: |_| {},
        bytecode: None,
        typelibs: Some("typelibs"),
    });
}
