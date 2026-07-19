use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use tokio::fs;

use dolang::{
    compile::{self, Compiler, Diag, EmitDiag, EmitToken, Mode, Severity},
    extension::CompilerExt,
    runtime::{
        Bytecode, Error, Result, Slot, Strand,
        error::{ErrorKind, ResultExt},
    },
};

use crate::{cli::PreludeImport, interactive::DYNAMIC_PRELUDE};

#[derive(Debug)]
struct Stop;

impl Display for Stop {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "compilation stopped due to too many errors")
    }
}

impl error::Error for Stop {}

pub(crate) async fn compile<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    path: &'a Path,
    source: &'a str,
    dynamic: Option<&[String]>,
    prelude: &[PreludeImport],
    mode: Mode<'a>,
    strict: bool,
) -> Result<'v, 's, Vec<u8>> {
    let mut out = Vec::new();
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut diagnostics = Vec::new();

    let compiler = compile_setup(path, source, dynamic, prelude, mode);

    let result = compiler.compile(&mut out, &mut |diag: Diag| -> ControlFlow<Stop> {
        match diag.severity() {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            _ => (),
        }
        diagnostics.push(diag);
        if errors > 10 {
            ControlFlow::Break(Stop)
        } else {
            ControlFlow::Continue(())
        }
    });
    let disp = path.display().to_string();
    for diag in &diagnostics {
        dolang_ext_shell::print_compile_diag_stderr(strand, &disp, source, diag).await?;
    }
    result.into_do(strand)?;
    if warnings != 0 && strict {
        Err(Error::compile(
            strand,
            "warnings treated as errors due to --strict flag",
        ))
    } else {
        Ok(out)
    }
}

fn compile_setup<'a>(
    path: &'a Path,
    source: &'a str,
    dynamic: Option<&'a [String]>,
    prelude: &[PreludeImport],
    mode: Mode<'a>,
) -> Compiler<'a> {
    let mut compiler = Compiler::new(Path::new(path), source.as_bytes());

    compiler.mode(mode);
    for ext in compiler.extensions() {
        ext.apply(&mut compiler).unwrap();
    }
    for import in prelude {
        match import {
            PreludeImport::Module { module, bind: None } => {
                compiler.prelude().import_module(module);
            }
            PreludeImport::Module {
                module,
                bind: Some(bind),
            } => {
                compiler.prelude().import_module_with_name(module, bind);
            }
            PreludeImport::Item {
                module,
                item,
                bind: None,
            } => {
                compiler.prelude().import_items(module).item(item).commit();
            }
            PreludeImport::Item {
                module,
                item,
                bind: Some(bind),
            } => {
                compiler
                    .prelude()
                    .import_items(module)
                    .item_with_name(item, bind)
                    .commit();
            }
        }
    }
    if let Some(dynamic) = dynamic {
        compiler
            .prelude()
            .import_items(DYNAMIC_PRELUDE)
            .items(dynamic)
            .commit();
    }

    compiler
}

pub(crate) fn analyze<'a, D: EmitDiag, T: EmitToken<Break = D::Break>>(
    path: &'a Path,
    source: &'a str,
    dynamic: Option<&[String]>,
    prelude: &[PreludeImport],
    diags: &mut D,
    tokens: &mut T,
) -> std::result::Result<(), compile::Error<D::Break>> {
    let compiler = compile_setup(path, source, dynamic, prelude, Mode::Repl);
    compiler.analyze(diags, tokens)
}

async fn file_is_newer(older: &Path, newer: &Path) -> bool {
    let older = fs::metadata(older).await.and_then(|older| older.modified());
    let newer = fs::metadata(newer).await.and_then(|newer| newer.modified());
    older
        .and_then(|older| newer.map(|newer| newer > older))
        .unwrap_or(false)
}

pub(crate) fn dirs<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, ProjectDirs> {
    ProjectDirs::from("", "", "dolang")
        .ok_or_else(|| Error::runtime(strand, "can't locate application directories"))
}

fn get_module_search_paths<'v, 's>(
    strand: &mut Strand<'v, 's>,
    module_paths: &[PathBuf],
) -> Result<'v, 's, Vec<PathBuf>> {
    let mut paths = module_paths.to_vec();
    paths.push(dirs(strand)?.data_dir().join("site"));
    Ok(paths)
}

pub(crate) async fn find_module_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    module_paths: &[PathBuf],
) -> Result<'v, 's, PathBuf> {
    let search_paths = get_module_search_paths(strand, module_paths)?;
    let mut relative_path = PathBuf::new();

    relative_path.extend(name.split('.'));
    let mut relative_path_alt = relative_path.clone();
    relative_path.set_extension("dol");
    relative_path_alt.push("mod.dol");

    for base_path in search_paths {
        for relative_path in [&relative_path, &relative_path_alt].into_iter() {
            let mut module_path = base_path.clone();
            module_path.extend(relative_path);

            if fs::try_exists(&module_path).await.into_do(strand)? {
                return Ok(module_path);
            }
        }
    }

    Err(Error::import(strand, name))
}

async fn compile_script<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, Vec<u8>> {
    if fs::try_exists(path).await.into_do(strand)? {
        let source = fs::read_to_string(path).await.into_do(strand)?;
        compile(strand, path, &source, None, prelude, Mode::Script, strict).await
    } else {
        Err(Error::runtime(
            strand,
            format!("could not find file: {}", path.display()),
        ))
    }
}

pub(crate) async fn compile_only<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, ()> {
    compile_script(strand, path, prelude, strict).await?;
    Ok(())
}

pub(crate) async fn compile_to_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    output: &Path,
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, ()> {
    let data = compile_script(strand, path, prelude, strict).await?;
    fs::write(output, &data).await.into_do(strand)?;
    Ok(())
}

pub(crate) async fn load<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    mode: Mode<'_>,
    prelude: &[PreludeImport],
    strict: bool,
    cache: bool,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let bc = cache
        .then(|| cache_path(strand, path, &mode, prelude, strict))
        .transpose()?;

    if let Some(bc) = &bc
        && fs::try_exists(bc).await.into_do(strand)?
        && !file_is_newer(bc, path).await
    {
        let data = fs::read(bc).await.into_do(strand)?;
        let bytecode = Bytecode::new(data);
        match bytecode.run(strand, &mut out).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Bytecode => (),
            Err(e) => return Err(e),
        }
    }
    let source = fs::read_to_string(path).await.into_do(strand)?;
    let data = compile(strand, path, &source, None, prelude, mode, strict).await?;
    if let Some(bc) = &bc {
        fs::create_dir_all(bc.parent().unwrap())
            .await
            .into_do(strand)?;
        fs::write(bc, &data).await.into_do(strand)?;
    }
    let bytecode = Bytecode::new(data);
    bytecode.run(strand, &mut out).await?;
    Ok(())
}

fn cache_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    mode: &Mode<'_>,
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, PathBuf> {
    let mut bc = dirs(strand)?.cache_dir().join("bytecode").clone();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"dolang-shell-cache-v2");
    hash_bytes(&mut hasher, path.as_os_str().as_encoded_bytes());
    match mode {
        Mode::Script => {
            hasher.update(b"script");
        }
        Mode::Module { name } => {
            hasher.update(b"module");
            hash_string(&mut hasher, name);
        }
        Mode::Repl => {
            hasher.update(b"repl");
        }
        _ => {
            hasher.update(b"unknown");
        }
    }
    hasher.update(&[u8::from(strict)]);
    for import in prelude {
        match import {
            PreludeImport::Module { module, bind } => {
                hasher.update(b"module");
                hash_string(&mut hasher, module);
                hash_optional_string(&mut hasher, bind.as_deref());
            }
            PreludeImport::Item { module, item, bind } => {
                hasher.update(b"item");
                hash_string(&mut hasher, module);
                hash_string(&mut hasher, item);
                hash_optional_string(&mut hasher, bind.as_deref());
            }
        }
    }
    bc.push(hasher.finalize().to_hex().as_str());
    bc.set_extension("dolc");
    Ok(bc)
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_bytes(hasher, value.as_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hash_optional_string(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(b"some");
            hash_string(hasher, value);
        }
        None => {
            hasher.update(b"none");
        }
    }
}
