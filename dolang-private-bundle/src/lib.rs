#![deny(warnings)]

use std::{collections::BTreeMap, fs, io::Write, path::Path, path::PathBuf};

use dolang_compile::{Config, Mode, diag::Severity, typeck};

mod render;

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

/// A tree of sources to bundle, and how to compile them.
pub struct Root<'a> {
    pub dir: &'a Path,
    pub virtual_root: &'a str,
    /// Prepares the compiler, e.g. with the prelude the sources run under.
    pub configure: fn(&mut Config<'_>),
    /// Whether the sources' bytecode is bundled, which requires [`Bundle::bytecode`].
    pub bytecode: bool,
    /// Whether to bundle a source, by its path relative to `dir`.
    pub select: &'a dyn Fn(&Path) -> bool,
}

/// What to bundle from trees of sources, and where.
///
/// Each output named here is written to `<out_dir>/<name>/`. A combined
/// `<out_dir>/resources.rs` holds a `&[dolang::resource::Resource]` expression
/// for the producing crate to include.
pub struct Bundle<'a> {
    pub roots: &'a [Root<'a>],
    pub out_dir: &'a Path,
    pub compile_mode: CompileMode,
    pub name_mode: NameMode,
    /// The output for bytecode.
    pub bytecode: Option<&'a str>,
    /// The output for typelibs, which requires `CompileMode::Module`. Every
    /// selected source has one, and each module a typelib imports must be bundled
    /// too.
    pub typelibs: Option<&'a str>,
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

/// An output's artifact directory and resource namespace.
struct Output {
    dir: PathBuf,
    namespace: &'static str,
}

impl Output {
    fn new(out_dir: &Path, name: &str, namespace: &'static str) -> Self {
        let dir = out_dir.join(name);
        fs::create_dir_all(&dir).unwrap();
        Self { dir, namespace }
    }

    fn add(&self, table: &mut fs::File, name: &str, ext: &str, bytes: &[u8]) {
        let path = self.dir.join(format!("{name}.{ext}"));
        fs::write(&path, bytes).unwrap();
        writeln!(
            table,
            "    ::dolang::resource::Resource {{ namespace: {:?}, name: {name:?}, \
             payload: ::dolang::resource::Payload::Bytes(include_bytes!({path:?})) }},",
            self.namespace,
        )
        .unwrap();
    }
}

pub fn bundle(spec: &Bundle<'_>) {
    assert!(
        spec.typelibs.is_none() || matches!(spec.compile_mode, CompileMode::Module),
        "only modules have typelibs"
    );
    let bytecode_namespace = match spec.compile_mode {
        CompileMode::Module => "dolang.module",
        CompileMode::Script => "dolang.entrypoint",
    };
    let bytecode_out = spec
        .bytecode
        .map(|name| Output::new(spec.out_dir, name, bytecode_namespace));
    let typelib_out = spec
        .typelibs
        .map(|name| Output::new(spec.out_dir, name, "dolang.typelib"));
    let mut table = fs::File::create(spec.out_dir.join("resources.rs")).unwrap();
    writeln!(table, "&[").unwrap();

    // Each selected source by module name, so a name is bundled once.
    let mut sources = BTreeMap::new();
    for root in spec.roots {
        assert!(
            !root.bytecode || spec.bytecode.is_some(),
            "a root's bytecode needs an output"
        );
        let mut files = Vec::new();
        walk_dol_files(root.dir, &mut files);
        for path in files {
            if !(root.select)(path.strip_prefix(root.dir).unwrap()) {
                continue;
            }
            let name = derive_name(&path, root.dir, spec.name_mode);
            if let Some((other, _)) = sources.insert(name.clone(), (path.clone(), root)) {
                panic!(
                    "{} and {} are both module {name}",
                    other.display(),
                    path.display()
                );
            }
        }
    }

    let mut imports = Vec::new();
    for (name, (path, root)) in &sources {
        println!("cargo::rerun-if-changed={}", path.display());

        let source = fs::read_to_string(path).unwrap();
        let relative = path.strip_prefix(root.dir).unwrap();
        let compiler_path = Path::new(root.virtual_root).join(relative);

        let mut config = Config::new();
        match spec.compile_mode {
            CompileMode::Module => config.mode(Mode::Module {
                name: name.as_str().into(),
            }),
            CompileMode::Script => config.mode(Mode::Script),
        };
        config.typecheck(typelib_out.is_some());
        (root.configure)(&mut config);

        let mut had_error = false;
        let mut had_warning = false;
        let compiler_path_str = compiler_path.display().to_string();
        let unit = config.unit(&compiler_path, source.as_str());
        for diag in unit.diagnostics() {
            match diag.severity() {
                Severity::Error => had_error = true,
                Severity::Warning => had_warning = true,
                _ => {}
            }
            eprintln!(
                "{}",
                render::render_diag(&compiler_path_str, &source, &diag)
            );
        }
        if had_error {
            panic!("compilation errors in {}", compiler_path.display());
        }
        if had_warning {
            panic!("compilation warnings in {}", compiler_path.display());
        }

        if let Some(out) = &typelib_out {
            let typelib = typeck::typelib(&unit)
                .unwrap_or_else(|err| panic!("no typelib for {}: {err}", compiler_path.display()));
            let read = typeck::Typelib::read(&typelib).unwrap();
            imports.extend(
                read.imports()
                    .into_iter()
                    .map(|import| (name, import.to_owned())),
            );
            out.add(&mut table, name, "dolt", &typelib);
        }
        if let Some(out) = &bytecode_out
            && root.bytecode
        {
            let mut bytecode = Vec::new();
            unit.emit(&mut bytecode)
                .unwrap_or_else(|_| panic!("failed to compile {}", compiler_path.display()));
            out.add(&mut table, name, "dolc", &bytecode);
        }
    }

    for (name, import) in imports {
        assert!(
            sources.contains_key(&import),
            "{name} imports {import}, which isn't bundled"
        );
    }

    writeln!(table, "]").unwrap();
}
