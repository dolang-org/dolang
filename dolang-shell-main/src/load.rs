use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use tokio::fs;

use dolang::{
    compile::{self, Config, Mode, Severity},
    extension::CompilerExt,
    runtime::{
        Bytecode, Error, Result, Slot, Strand,
        error::{ErrorKind, ResultExt},
    },
};

use crate::{cli::PreludeImport, interactive::DYNAMIC_PRELUDE};

/// Maximum number of errors reported for a single compilation
const MAX_ERRORS: usize = 10;

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

    let unit = compile_setup(dynamic, prelude, mode).unit(path, source.as_bytes());

    for diag in unit.diagnostics() {
        match diag.severity() {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            _ => (),
        }
        diagnostics.push(diag);
        if errors > MAX_ERRORS {
            break;
        }
    }
    let result = unit.emit(&mut out);
    let disp = path.display().to_string();
    for diag in &diagnostics {
        dolang_ext_shell::print_compile_diag_stderr(strand, &disp, source, diag).await?;
    }
    if let Err(error) = result {
        return Err(Error::compile(strand, error));
    }
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
    dynamic: Option<&[String]>,
    prelude: &[PreludeImport],
    mode: Mode<'a>,
) -> Config<'a> {
    let mut config = Config::new();

    config.mode(mode);
    for ext in config.extensions() {
        ext.apply(&mut config).unwrap();
    }
    for import in prelude {
        match import {
            PreludeImport::Module { module, bind: None } => {
                config.prelude().import_module(module);
            }
            PreludeImport::Module {
                module,
                bind: Some(bind),
            } => {
                config.prelude().import_module_with_name(module, bind);
            }
            PreludeImport::Item {
                module,
                item,
                bind: None,
            } => {
                config.prelude().import_items(module).item(item).commit();
            }
            PreludeImport::Item {
                module,
                item,
                bind: Some(bind),
            } => {
                config
                    .prelude()
                    .import_items(module)
                    .item_with_name(item, bind)
                    .commit();
            }
        }
    }
    if let Some(dynamic) = dynamic {
        config
            .prelude()
            .import_items(DYNAMIC_PRELUDE)
            .items(dynamic)
            .commit();
    }

    config
}

/// Build a REPL compilation unit for analysis, recovering from errors so that tokens
/// remain available for incomplete input.
pub(crate) fn unit<'a>(
    path: &'a Path,
    source: &'a str,
    dynamic: Option<&[String]>,
    prelude: &[PreludeImport],
) -> compile::Unit<'a> {
    let mut config = compile_setup(dynamic, prelude, Mode::Repl);
    config.recover(true).document(true);
    config.unit(path, source.as_bytes())
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

/// Magic prefix of a compiled bytecode file.
const BYTECODE_MAGIC: [u8; 8] = *b"\xffdobytec";

/// Extension of a compiled bytecode file.
const BYTECODE_EXTENSION: &str = "dolc";

fn has_bytecode_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == BYTECODE_EXTENSION)
}

fn is_bytecode(path: &Path, data: &[u8]) -> bool {
    has_bytecode_extension(path) || data.starts_with(&BYTECODE_MAGIC)
}

/// Determine whether `path` holds pre-compiled bytecode without reading it whole.
///
/// A missing or unreadable file is reported as not being bytecode so the
/// regular compile path produces the usual error.
async fn is_precompiled(path: &Path) -> bool {
    use tokio::io::AsyncReadExt as _;

    if has_bytecode_extension(path) {
        return true;
    }
    let Ok(mut file) = fs::File::open(path).await else {
        return false;
    };
    let mut header = [0u8; BYTECODE_MAGIC.len()];
    file.read_exact(&mut header).await.is_ok() && header == BYTECODE_MAGIC
}

async fn compile_script<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, Vec<u8>> {
    if fs::try_exists(path).await.into_do(strand)? {
        let data = fs::read(path).await.into_do(strand)?;
        if is_bytecode(path, &data) {
            return Ok(data);
        }
        let source = String::from_utf8(data)
            .map_err(|_| Error::runtime(strand, format!("not valid UTF-8: {}", path.display())))?;
        compile(strand, path, &source, None, prelude, Mode::Script, strict).await
    } else {
        Err(Error::runtime(
            strand,
            format!("could not find file: {}", path.display()),
        ))
    }
}

pub(crate) async fn compile_script_cached<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    prelude: &[PreludeImport],
    strict: bool,
    cache: bool,
) -> Result<'v, 's, Vec<u8>> {
    // Pre-compiled input needs neither compilation nor caching.
    if is_precompiled(path).await {
        return fs::read(path).await.into_do(strand);
    }

    let mode = Mode::Script;
    let bc = cache
        .then(|| cache_path(strand, path, &mode, prelude, strict))
        .transpose()?;

    if let Some(data) = read_cached(strand, path, bc.as_deref()).await? {
        return Ok(data);
    }

    let data = compile_script(strand, path, prelude, strict).await?;
    write_cached(strand, bc.as_deref(), &data).await?;
    Ok(data)
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
    // A script given as pre-compiled bytecode is run as-is.
    if matches!(mode, Mode::Script) && is_precompiled(path).await {
        let data = fs::read(path).await.into_do(strand)?;
        return Bytecode::new(data).run(strand, &mut out).await;
    }

    let bc = cache
        .then(|| cache_path(strand, path, &mode, prelude, strict))
        .transpose()?;

    if let Some(data) = read_cached(strand, path, bc.as_deref()).await? {
        let bytecode = Bytecode::new(data);
        match bytecode.run(strand, &mut out).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Bytecode => (),
            Err(e) => return Err(e),
        }
    }
    let source = fs::read_to_string(path).await.into_do(strand)?;
    let data = compile(strand, path, &source, None, prelude, mode, strict).await?;
    write_cached(strand, bc.as_deref(), &data).await?;
    let bytecode = Bytecode::new(data);
    bytecode.run(strand, &mut out).await?;
    Ok(())
}

async fn read_cached<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    bc: Option<&Path>,
) -> Result<'v, 's, Option<Vec<u8>>> {
    if let Some(bc) = bc
        && fs::try_exists(bc).await.into_do(strand)?
        && !file_is_newer(bc, path).await
    {
        Ok(Some(fs::read(bc).await.into_do(strand)?))
    } else {
        Ok(None)
    }
}

async fn write_cached<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bc: Option<&Path>,
    data: &[u8],
) -> Result<'v, 's, ()> {
    if let Some(bc) = bc {
        fs::create_dir_all(bc.parent().unwrap())
            .await
            .into_do(strand)?;
        fs::write(bc, data).await.into_do(strand)?;
    }
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
