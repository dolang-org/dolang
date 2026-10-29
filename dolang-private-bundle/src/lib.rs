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
    /// The typelibs, which require `CompileMode::Module`. Every selected source
    /// has one, written from a check of them all, which must report nothing.
    /// Each module a typelib imports must be bundled too.
    pub typelibs: Option<Typelibs<'a>>,
}

/// Where to write typelibs, and what their check assumes
pub struct Typelibs<'a> {
    /// The output
    pub name: &'a str,
    /// The pipe types the check nominates, sender then receiver, each by module
    /// and item (see `typeck::Builder::pipes`). The checks that read the typelibs
    /// must nominate the same.
    pub pipes: [(&'a str, &'a str); 2],
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
    let typelib_out = (spec.typelibs.as_ref()).map(|typelibs| {
        let out = Output::new(spec.out_dir, typelibs.name, "dolang.typelib");
        (out, typelibs.pipes)
    });
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

    // Each source's path as the compiler sees it, and its text, which its unit
    // borrows
    let texts: Vec<(String, String)> = sources
        .values()
        .map(|(path, root)| {
            println!("cargo::rerun-if-changed={}", path.display());
            let relative = path.strip_prefix(root.dir).unwrap();
            let compiler_path = Path::new(root.virtual_root).join(relative);
            (
                compiler_path.display().to_string(),
                fs::read_to_string(path).unwrap(),
            )
        })
        .collect();
    let mut units = Vec::new();
    for ((name, (_, root)), (compiler_path, source)) in sources.iter().zip(&texts) {
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
        let unit = config.unit(Path::new(compiler_path), source.as_str());
        for diag in unit.diagnostics() {
            match diag.severity() {
                Severity::Error => had_error = true,
                Severity::Warning => had_warning = true,
                _ => {}
            }
            eprintln!("{}", render::render_diag(compiler_path, &unit, &diag));
        }
        if had_error {
            panic!("compilation errors in {compiler_path}");
        }
        if had_warning {
            panic!("compilation warnings in {compiler_path}");
        }
        units.push(unit);
    }

    // The modules are checked together, and each one's typelib written from the
    // check
    if let Some((out, [sender, receiver])) = &typelib_out {
        let mut builder = typeck::Builder::new();
        builder.typelibs(true).pipes(*sender, *receiver);
        let ids: Vec<_> = units
            .iter()
            .map(|unit| builder.unit(unit).unwrap())
            .collect();
        let check = builder.check();
        let mut failed = false;
        for diag in check.diagnostics() {
            let unit = diag
                .span()
                .unit()
                .expect("checker diagnostics name their unit");
            let index = ids.iter().position(|&id| id == unit).unwrap();
            let (compiler_path, _) = &texts[index];
            eprintln!(
                "{}",
                render::render_diag(compiler_path, &units[index], diag)
            );
            failed |= matches!(diag.severity(), Severity::Error | Severity::Warning);
        }
        if failed {
            panic!("the bundled modules don't check");
        }
        for ((name, id), (compiler_path, _)) in sources.keys().zip(&ids).zip(&texts) {
            let mut typelib = Vec::new();
            check
                .typelib(*id, &mut typelib)
                .unwrap_or_else(|err| panic!("no typelib for {compiler_path}: {err}"));
            out.add(&mut table, name, "dolt", &typelib);
        }
    }

    if let Some(out) = &bytecode_out {
        for (((name, (_, root)), (compiler_path, _)), unit) in sources.iter().zip(&texts).zip(units)
        {
            if !root.bytecode {
                continue;
            }
            let mut bytecode = Vec::new();
            unit.emit(&mut bytecode)
                .unwrap_or_else(|_| panic!("failed to compile {compiler_path}"));
            out.add(&mut table, name, "dolc", &bytecode);
        }
    }

    writeln!(table, "]").unwrap();
}
