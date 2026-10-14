use std::convert::Infallible;

use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

use crate::{global::Global, term};

/// Terminal extension
pub struct TermExt;

impl Extension for TermExt {
    type Error = Infallible;
    const NAME: &str = "term";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Terminal Extension";

    fn apply_compiler(&self, config: &mut Config) -> Result<(), Infallible> {
        term::configure_compiler(config);
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        term::configure_vm(builder, global);
        Ok(())
    }
}

extension!(TermExt);
