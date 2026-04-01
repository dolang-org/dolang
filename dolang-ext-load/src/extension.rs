use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
};

use dolang::{
    compile::Compiler,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

/// Load extension
pub struct LoadExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for LoadExt {
    type Error = Infallible;
    const NAME: &str = "dolang-load";
    const VERSION: Version = Version {
        major: 0,
        minor: 1,
        patch: 0,
    };
    const DESCRIPTION: &str = "Do Load Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        crate::load::configure(builder);
        Ok(())
    }
}

extension!(LoadExt);
