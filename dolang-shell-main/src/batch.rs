use std::path::{Path, PathBuf};

use dolang::{
    compile::Mode,
    runtime::{Bytecode, Result, Strand},
};

use crate::{cli::PreludeImport, load};

pub enum Action {
    Run,
    /// Check the script, finding modules on the module paths, else among the
    /// linked bundled typelibs
    Check {
        module_paths: Vec<PathBuf>,
    },
    Compile(PathBuf),
}

pub(crate) async fn main<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &Path,
    action: Action,
    entrypoint: Option<&'static [u8]>,
    prelude: &[PreludeImport],
    strict: bool,
    cache: bool,
) -> Result<'v, 's, ()> {
    match action {
        Action::Run => {
            strand
                .with_slots(async move |strand, [tmp]| {
                    if let Some(entrypoint) = entrypoint {
                        Bytecode::new(entrypoint).run(strand, tmp).await
                    } else {
                        load::load(strand, path, Mode::Script, prelude, strict, cache, tmp).await
                    }
                })
                .await
        }
        Action::Check { module_paths } => {
            load::check(strand, path, prelude, strict, &module_paths).await
        }
        Action::Compile(output) => {
            load::compile_to_file(strand, path, &output, prelude, strict).await
        }
    }
}
