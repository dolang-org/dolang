//! Native services used by bundled shell entrypoints.
use dolang::runtime::AllocExt;
use dolang::{
    compile::{Mode, Severity, Thresholds},
    resource::{self, Payload},
    runtime::{Error, io::IoResultExt as _, unpack, value::Nil, vm::Register},
};
use dolang_ext_compile::apply_prelude_value;

use crate::{interactive::LineEditor, load};

pub(crate) fn register<'v>(builder: &mut Register<'v>) {
    let prelude_sym = builder.sym("prelude");
    let editor = builder.build_type::<LineEditor>((), prelude_sym).build();
    let mode = builder.sym("mode");
    let diagnose_sym = builder.sym("diagnose");
    let fail_sym = builder.sym("fail");
    let lint_sym = builder.sym("LINT");
    let warning_sym = builder.sym("WARNING");
    let error_sym = builder.sym("ERROR");
    let script_sym = builder.sym("SCRIPT");
    let repl_sym = builder.sym("REPL");
    builder
        .module("_shell")
        .value("Editor", editor)
        .function("compile", async move |strand, args, out| {
            unpack!(strand, args =>
                path,
                ?source,
                ?:mode,
                ?prelude_sym: prelude,
                ?diagnose_sym: diagnose_arg,
                ?fail_sym: fail_arg
            );
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
            let level = |value: Option<Option<_>>, default| match value {
                None => Some(default),
                Some(Some(sym)) if sym == lint_sym => Some(Severity::Lint),
                Some(Some(sym)) if sym == warning_sym => Some(Severity::Warning),
                Some(Some(sym)) if sym == error_sym => Some(Severity::Error),
                Some(_) => None,
            };
            let diagnose = diagnose_arg.map(|value| value.as_sym(strand));
            let fail = fail_arg.map(|value| value.as_sym(strand));
            let defaults = Thresholds::default();
            let (Some(diagnose), Some(fail)) = (
                level(diagnose, Severity::Warning),
                level(fail, defaults.fail_level()),
            ) else {
                return Err(Error::type_error(
                    strand,
                    "diagnose and fail must be :LINT:, :WARNING:, or :ERROR:",
                ));
            };
            let thresholds = Thresholds::new(diagnose, fail);
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
                None => tokio::fs::read_to_string(&path).await.into_do_io(strand)?,
            };
            let data = load::compile_config(strand, &path, &source, &config, thresholds).await?;
            strand.store(out, data.as_slice());
            Ok(())
        })
        .function("resource", async move |strand, args, out| {
            unpack!(strand, args => namespace, name);
            let namespace = namespace
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "namespace must be Str"))?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be Str"))?;
            let payload = strand
                .access(|access| resource::get(namespace.as_str(access), name.as_str(access)));
            match payload {
                Some(Payload::Text(text)) => strand.store(out, text),
                Some(Payload::Bytes(bytes)) => strand.store(out, bytes),
                None => strand.store(out, Nil),
            }
            Ok(())
        })
        .commit();
}
