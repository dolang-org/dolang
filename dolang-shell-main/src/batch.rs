use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use dolang::{
    compile::Mode,
    runtime::{Bytecode, Result, Strand},
};

use crate::{Config, cli::PreludeImport, load};

pub enum Action {
    Run,
    /// Check the script, finding modules on the module paths, else among the
    /// configuration's bundled typelibs
    Check {
        module_paths: Vec<PathBuf>,
        config: Arc<dyn Config>,
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
        Action::Check {
            module_paths,
            config,
        } => {
            let bundled_typelib = |name: &str| crate::bundled_typelib(&*config, name);
            load::check(
                strand,
                path,
                prelude,
                strict,
                &module_paths,
                bundled_typelib,
            )
            .await
        }
        Action::Compile(output) => {
            load::compile_to_file(strand, path, &output, prelude, strict).await
        }
    }
}
