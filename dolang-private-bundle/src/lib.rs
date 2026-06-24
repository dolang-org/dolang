#![deny(warnings)]

extern crate dolang_ext_shell;

use std::{fs, io::Write, ops::ControlFlow, path::Path, path::PathBuf};

use dolang::{
    compile::{Compiler, Mode, Severity},
    extension::CompilerExt,
};

#[derive(Clone, Copy, Debug)]
pub enum CompileMode {
    Module,
    Script,
}

#[derive(Clone, Copy, Debug)]
pub enum NameMode {
    Module,
    Stem,
}

pub struct Bundle<'a> {
    pub source_root: &'a Path,
    pub out_dir: &'a Path,
    pub output_name: &'a str,
    pub table_name: &'a str,
    pub virtual_root: &'a str,
    pub compile_mode: CompileMode,
    pub name_mode: NameMode,
}

fn walk_dol_files(dir: &Path, files: &mut Vec<PathBuf>) {
    println!("cargo::rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk_dol_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "dol") {
            files.push(path);
        }
    }
}

fn derive_name(path: &Path, source_root: &Path, name_mode: NameMode) -> String {
    let relative = path.strip_prefix(source_root).unwrap();
    let mut components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_str().unwrap().to_owned())
        .collect();
    if let Some(last) = components.last_mut()
        && let Some(stem) = last.strip_suffix(".dol")
    {
        *last = stem.to_owned();
    }
    if matches!(name_mode, NameMode::Module)
        && components.len() > 1
        && components
            .last()
            .is_some_and(|component| component == "mod")
    {
        components.pop();
    }
    components.join(".")
}

pub fn bundle(spec: &Bundle<'_>) {
    let bundled_dir = spec.out_dir.join(spec.output_name);
    fs::create_dir_all(&bundled_dir).unwrap();

    let mut files = Vec::new();
    walk_dol_files(spec.source_root, &mut files);
    files.sort();

    let generated_path = spec.out_dir.join(format!("{}.rs", spec.output_name));
    let mut generated = fs::File::create(&generated_path).unwrap();
    writeln!(
        generated,
        "pub static {}: &[(&str, &[u8])] = &[",
        spec.table_name
    )
    .unwrap();

    for path in &files {
        println!("cargo::rerun-if-changed={}", path.display());

        let name = derive_name(path, spec.source_root, spec.name_mode);
        let source = fs::read_to_string(path).unwrap();
        let relative = path.strip_prefix(spec.source_root).unwrap();
        let compiler_path = Path::new(spec.virtual_root).join(relative);

        let mut compiler = Compiler::new(&compiler_path, source.as_bytes());
        match spec.compile_mode {
            CompileMode::Module => compiler.mode(Mode::Module { name: &name }),
            CompileMode::Script => compiler.mode(Mode::Script),
        };
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

        let bc_path = bundled_dir.join(format!("{name}.dolc"));
        fs::write(&bc_path, &bytecode).unwrap();
        writeln!(
            generated,
            "    ({:?}, include_bytes!({:?})),",
            name, bc_path
        )
        .unwrap();
    }

    writeln!(generated, "];").unwrap();
}
