use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
};

use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

/// Compile extension
pub struct CompileExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for CompileExt {
    type Error = Infallible;
    const NAME: &str = "dolang-compile";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Compile Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<crate::compile::Tag>(&["compile"], |reg| {
            let global = crate::compile::Global::new(reg);
            let global = reg.register_state(global);
            crate::compile::configure(reg, global);
        });
        Ok(())
    }
}

extension!(CompileExt);
