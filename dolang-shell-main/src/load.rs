//! Compiler setup and diagnostics for native shell services.
use std::path::Path;

use dolang::{
    compile::{Config, Mode, Severity, Thresholds},
    extension::CompilerExt,
    runtime::{Error, Result, Strand},
};

const MAX_ERRORS: usize = 10;

pub(crate) async fn compile_config<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    source: &str,
    config: &Config<'_>,
    thresholds: Thresholds,
) -> Result<'v, 's, Vec<u8>> {
    let mut out = Vec::new();
    let mut errors = 0usize;
    let mut failures = 0usize;
    let mut diagnostics = Vec::new();

    let unit = config.unit(path, source);

    for diag in unit.diagnostics() {
        if diag.severity() == Severity::Error {
            errors += 1;
        }
        if thresholds.fails(diag.severity()) {
            failures += 1;
        }
        if thresholds.shows(diag.severity()) {
            diagnostics.push(diag);
        }
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
    if failures != 0 {
        let message = format!(
            "diagnostics at or above {} treated as errors due to --fail flag",
            thresholds.fail_level()
        );
        Err(Error::compile(strand, message))
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
