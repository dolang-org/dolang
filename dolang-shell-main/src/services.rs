//! Native services used by bundled shell entrypoints.
use dolang::{
    compile::Mode,
    resource::{self, Payload},
    runtime::{Error, Output, error::ResultExt, unpack, value::Nil, vm::Register},
};
use dolang_ext_compile::apply_prelude_value;

use crate::{interactive::LineEditor, load};

pub(crate) fn register<'v>(builder: &mut Register<'v>) {
    let prelude_sym = builder.sym("prelude");
    let editor = builder.build_type::<LineEditor>((), prelude_sym).build();
    let mode_sym = builder.sym("mode");
    let strict_sym = builder.sym("strict");
    let backtrace_sym = builder.sym("backtrace");
    let script_sym = builder.sym("SCRIPT");
    let repl_sym = builder.sym("REPL");
    builder
        .module("_shell")
        .value("Editor", editor)
        .function("render_error", async move |strand, args, out| {
            let ([error], [backtrace]) = unpack!(strand, args, 1, 0, backtrace_sym = None)?;
            dolang_ext_shell::render_error(strand, &error, backtrace.as_deref(), out)
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
                .unwrap_or(false);
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
            let mut config = load::compile_setup(mode);
            if let Some(prelude) = &prelude {
                apply_prelude_value(strand, &mut config, prelude)?;
            }
            let source = match source {
                Some(source) => source,
                None => tokio::fs::read_to_string(&path).await.into_do(strand)?,
            };
            let data = load::compile_config(strand, &path, &source, &config, strict).await?;
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
