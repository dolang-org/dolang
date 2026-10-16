//! Native services used by bundled shell entrypoints.
use dolang::{
    compile::Mode,
    resource::{self, Payload},
    runtime::{
        Error, Output, Result, Slot, Strand,
        error::ResultExt,
        unpack,
        value::{AsSym, Empty, Nil},
        vm::Register,
    },
};
use dolang_ext_compile::apply_prelude_value;
use dolang_vfs::path::WellKnownPath;

use crate::{
    cli::{Cli, PreludeImport},
    interactive::LineEditor,
    load,
};

pub(crate) fn register<'v>(builder: &mut Register<'v>, cli: &Cli) {
    let prelude_sym = builder.sym("prelude");
    let editor = builder.build_type::<LineEditor>((), prelude_sym).build();
    let mode_sym = builder.sym("mode");
    let strict_sym = builder.sym("strict");
    let backtrace_sym = builder.sym("backtrace");
    let script_sym = builder.sym("SCRIPT");
    let repl_sym = builder.sym("REPL");
    let compile_prelude = cli.prelude.clone();
    let options_prelude = cli.prelude.clone();
    let strict = cli.strict;
    let cache = cli.cache;
    let history = cli.history;
    builder
        .module("_shell")
        .value("Editor", editor)
        .function("render_error", async move |strand, args, out| {
            let ([error], [backtrace]) = unpack!(strand, args, 1, 0, backtrace_sym = None)?;
            dolang_ext_shell::render_error(strand, &error, backtrace.as_deref(), out)
        })
        .function("repl_options", async move |strand, args, mut out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let state_dir = if history {
                load::directory(strand, WellKnownPath::StateDir).await?
            } else {
                None
            };
            let data_local_dir = if history {
                Some(load::required_directory(strand, WellKnownPath::DataLocalDir).await?)
            } else {
                None
            };
            strand.with_slots_sync(|strand, [mut prelude]| {
                prelude_value(strand, &options_prelude, Slot::reborrow(&mut prelude))?;
                Output::set(strand, &mut out, Empty::Dict);
                let dict = out.as_dict(strand).unwrap();
                dict.insert(strand, AsSym::new("prelude"), &prelude, true)?;
                dict.insert(strand, AsSym::new("strict"), strict, true)?;
                dict.insert(strand, AsSym::new("history"), history, true)?;
                dict.insert(strand, AsSym::new("state_dir"), Nil, true)?;
                dict.insert(strand, AsSym::new("data_local_dir"), Nil, true)?;
                if let Some(path) = &state_dir {
                    dict.insert(
                        strand,
                        AsSym::new("state_dir"),
                        path.to_string_lossy().as_ref(),
                        true,
                    )?;
                }
                if let Some(path) = &data_local_dir {
                    dict.insert(
                        strand,
                        AsSym::new("data_local_dir"),
                        path.to_string_lossy().as_ref(),
                        true,
                    )?;
                }
                Ok(())
            })
        })
        .function("compile", async move |strand, args, out| {
            let ([path], [source, mode, prelude, strict_arg]) = unpack!(
                strand,
                args,
                1,
                1,
                mode_sym = None,
                prelude_sym = None,
                strict_sym = None
            )?;
            let path = dolang_ext_shell::as_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "path must be Str or fs.Path"))?;
            let mode = match mode {
                None => Mode::Script,
                Some(value) if value.as_sym(strand) == Some(script_sym) => Mode::Script,
                Some(value) if value.as_sym(strand) == Some(repl_sym) => Mode::Repl,
                Some(value) => Mode::Module {
                    name: value
                        .as_str(strand)
                        .ok_or_else(|| {
                            Error::type_error(
                                strand,
                                "mode must be :SCRIPT:, :REPL:, or a module name",
                            )
                        })?
                        .to_string()
                        .into(),
                },
            };
            let strict = strict_arg
                .map(|value| value.to_bool(strand))
                .transpose()?
                .unwrap_or(strict);
            let source = source
                .map(|value| {
                    value
                        .as_str(strand)
                        .map(|text| text.to_string())
                        .ok_or_else(|| Error::type_error(strand, "source must be Str"))
                })
                .transpose()?;
            if mode == Mode::Repl && source.is_none() {
                return Err(Error::value(strand, "REPL mode requires source"));
            }
            let mut config = load::compile_setup(
                if prelude.is_some() {
                    &[]
                } else {
                    &compile_prelude
                },
                mode.clone(),
            );
            if let Some(prelude) = &prelude {
                apply_prelude_value(strand, &mut config, prelude)?;
            }
            // Precompiled files are passed through, independent of compiler settings.
            if mode != Mode::Repl && source.is_none() && load::is_precompiled(&path).await {
                let data = tokio::fs::read(&path).await.into_do(strand)?;
                Output::set(strand, out, data.as_slice());
                return Ok(());
            }
            let cache_path = if cache && mode != Mode::Repl {
                let mut hash = blake3::Hasher::new();
                hash.update(b"dolang-shell-compile-v1");
                hash_field(&mut hash, path.as_os_str().as_encoded_bytes());
                match &mode {
                    Mode::Script => {
                        hash.update(b"script");
                    }
                    Mode::Module { name } => {
                        hash.update(b"module");
                        hash_field(&mut hash, name.as_bytes());
                    }
                    _ => unreachable!(),
                }
                hash.update(&[u8::from(strict)]);
                if let Some(prelude) = &prelude {
                    hash_field(&mut hash, prelude.to_debug(strand)?.as_bytes());
                } else {
                    hash_field(&mut hash, format!("{compile_prelude:?}").as_bytes());
                }
                if let Some(source) = &source {
                    hash.update(b"source");
                    hash_field(&mut hash, source.as_bytes());
                } else {
                    hash.update(b"file");
                }
                Some(
                    load::required_directory(strand, WellKnownPath::CacheDir)
                        .await?
                        .join("bytecode")
                        .join(hash.finalize().to_hex().as_str())
                        .with_extension("dolc"),
                )
            } else {
                None
            };
            let cached = if source.is_some() {
                if let Some(cache_path) = &cache_path {
                    match tokio::fs::read(cache_path).await {
                        Ok(data) => Some(data),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(Error::runtime(strand, error)),
                    }
                } else {
                    None
                }
            } else {
                load::read_cached(strand, &path, cache_path.as_deref()).await?
            };
            let data = if let Some(data) = cached {
                data
            } else {
                let source = match source {
                    Some(source) => source,
                    None => tokio::fs::read_to_string(&path).await.into_do(strand)?,
                };
                let data = load::compile_config(strand, &path, &source, &config, strict).await?;
                load::write_cached(strand, cache_path.as_deref(), &data).await?;
                data
            };
            Output::set(strand, out, data.as_slice());
            Ok(())
        })
        .function("resource", async move |strand, args, out| {
            let ([namespace, name], []) = unpack!(strand, args, 2, 0)?;
            let namespace = namespace
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "namespace must be Str"))?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be Str"))?;
            let payload = strand
                .access(|access| resource::get(namespace.as_str(access), name.as_str(access)));
            match payload {
                Some(Payload::Text(text)) => Output::set(strand, out, text),
                Some(Payload::Bytes(bytes)) => Output::set(strand, out, bytes),
                None => Output::set(strand, out, Nil),
            }
            Ok(())
        })
        .commit();
}

fn hash_field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn prelude_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    imports: &[PreludeImport],
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    Output::set(strand, &mut out, Empty::Dict);
    let dict = out.as_dict(strand).unwrap();
    strand.with_slots_sync(|strand, [mut binding]| {
        let mut position = 0i64;
        for import in imports {
            if let PreludeImport::Module { module, bind: None } = import {
                dict.insert(strand, position, module.as_str(), false)?;
                position += 1;
                continue;
            }
            let module = match import {
                PreludeImport::Module { module, bind } => {
                    Output::set(strand, &mut binding, bind.as_deref().unwrap());
                    module
                }
                PreludeImport::Item { module, item, bind } => {
                    Output::set(strand, &mut binding, Empty::Dict);
                    binding.as_dict(strand).unwrap().insert(
                        strand,
                        item.as_str(),
                        bind.as_deref().unwrap_or(item),
                        false,
                    )?;
                    module
                }
            };
            dict.insert(strand, module.as_str(), &binding, false)?;
        }
        Ok(())
    })
}
