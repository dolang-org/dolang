use std::path::{Path, PathBuf};

use dolang::{
    compile::Mode,
    runtime::{Bytecode, Result, Strand},
};

use crate::cli::PreludeImport;
use crate::load;

pub enum Action {
    Run,
    Check,
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
                .with_slots(async move |strand, [_module, tmp]| {
                    if let Some(entrypoint) = entrypoint {
                        Bytecode::new(entrypoint).run(strand, tmp).await
                    } else {
                        load::load(strand, path, Mode::Script, prelude, strict, cache, tmp).await
                    }
                })
                .await
        }
        Action::Check => load::compile_only(strand, path, prelude, strict).await,
        Action::Compile(output) => {
            load::compile_to_file(strand, path, &output, prelude, strict).await
        }
    }
}
