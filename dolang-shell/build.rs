extern crate dolang_ext_shell;

use std::{env, fs, io::Write, ops::ControlFlow, path::Path};

use dolang::compile::{Compiler, Mode, Severity};
use dolang::extension::CompilerExt;

fn walk_dol_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    println!("cargo::rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk_dol_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "dol") {
            files.push(path);
        }
    }
}

fn derive_module_name(path: &Path, lib_root: &Path) -> String {
    let relative = path.strip_prefix(lib_root).unwrap();
    let mut components: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_str().unwrap().to_owned())
        .collect();
    // Strip .dol extension from last component
    if let Some(last) = components.last_mut()
        && let Some(stem) = last.strip_suffix(".dol")
    {
        *last = stem.to_owned();
    }
    // Strip trailing "mod" component (foo/mod.dol → foo)
    if components.len() > 1 && components.last().is_some_and(|s| s == "mod") {
        components.pop();
    }
    components.join(".")
}

fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);
    let lib_dir = Path::new(&manifest_dir).join("lib");

    let bundled_dir = out_dir.join("bundled");
    fs::create_dir_all(&bundled_dir).unwrap();

    let mut files = Vec::new();
    walk_dol_files(&lib_dir, &mut files);
    files.sort();

    let generated_path = out_dir.join("bundled_modules.rs");
    let mut generated = fs::File::create(&generated_path).unwrap();
    writeln!(
        generated,
        "pub static BUNDLED_MODULES: &[(&str, &[u8])] = &["
    )
    .unwrap();

    for path in &files {
        println!("cargo::rerun-if-changed={}", path.display());

        let module_name = derive_module_name(path, &lib_dir);
        let source = fs::read_to_string(path).unwrap();

        // Normalize the compiler path to <lib>/relative to avoid embedding
        // build machine absolute paths in debug symbols.
        let relative = path.strip_prefix(&lib_dir).unwrap();
        let compiler_path = Path::new("/<bundled>").join(relative);

        let mut compiler = Compiler::new(&compiler_path, source.as_bytes());
        compiler.mode(Mode::Module { name: &module_name });
        for ext in compiler.extensions() {
            ext.apply(&mut compiler).unwrap();
        }

        let mut bytecode = Vec::new();
        let mut had_error = false;
        let mut had_warning = false;
        compiler
            .compile(
                &mut bytecode,
                &mut |diag: dolang::compile::Diag| -> ControlFlow<()> {
                    let line = diag.span().start().line_number();
                    match diag.severity() {
                        Severity::Error => {
                            had_error = true;
                            eprintln!(
                                "error:{}:{}: {}",
                                compiler_path.display(),
                                line,
                                diag.message()
                            );
                        }
                        Severity::Warning => {
                            had_warning = true;
                            eprintln!(
                                "warning:{}:{}: {}",
                                compiler_path.display(),
                                line,
                                diag.message()
                            );
                        }
                        _ => {}
                    }
                    ControlFlow::Continue(())
                },
            )
            .unwrap_or_else(|_| panic!("failed to compile {}", compiler_path.display()));

        if had_error {
            panic!("compilation errors in {}", compiler_path.display());
        }
        if had_warning {
            panic!("compilation warnings in {}", compiler_path.display());
        }

        let bc_path = bundled_dir.join(format!("{module_name}.dolc"));
        fs::write(&bc_path, &bytecode).unwrap();

        writeln!(
            generated,
            "    ({:?}, include_bytes!({:?})),",
            module_name, bc_path
        )
        .unwrap();
    }

    writeln!(generated, "];").unwrap();
}
