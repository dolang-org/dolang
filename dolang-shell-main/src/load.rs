//! Compiler setup and diagnostics for native shell services.
use std::path::Path;

use dolang::{
    compile::{Config, Mode, Severity},
    extension::CompilerExt,
    runtime::{Error, Result, Strand},
};

const MAX_ERRORS: usize = 10;

pub(crate) async fn compile_config<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    source: &str,
    config: &Config<'_>,
    strict: bool,
) -> Result<'v, 's, Vec<u8>> {
    let mut out = Vec::new();
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut diagnostics = Vec::new();

    let unit = config.unit(path, source);

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
    let disp = path.display().to_string();
    for diag in &diagnostics {
        dolang_ext_shell::print_compile_diag_stderr(strand, &disp, &unit, diag).await?;
    }
    let result = unit.emit(&mut out);
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

pub(crate) fn compile_setup(mode: Mode<'_>) -> Config<'_> {
    let mut config = Config::new();
    config.mode(mode);
    for ext in config.extensions() {
        ext.apply(&mut config).unwrap();
    }
    config
}
