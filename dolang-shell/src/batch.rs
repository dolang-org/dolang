use std::path::{Path, PathBuf};

use dolang::{
    compile::Mode,
    runtime::{Result, Strand, Sym, method},
};

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
    main: Option<Sym<'v, 'v>>,
    strict: bool,
) -> Result<'v, 's, ()> {
    match action {
        Action::Run => {
            strand
                .with_slots(async move |strand, [mut module, tmp]| {
                    // Import the module (from path) and call its main function
                    if let Some(main) = main {
                        strand.import(&path.to_string_lossy(), &mut module).await?;
                        // Call the main method on the imported module
                        method!(strand, &module, main, tmp).await
                    } else {
                        load::load(strand, path, Mode::Script, strict, tmp).await
                    }
                })
                .await
        }
        Action::Check => load::compile_only(strand, path, strict).await,
        Action::Compile(output) => load::compile_to_file(strand, path, &output, strict).await,
    }
}
