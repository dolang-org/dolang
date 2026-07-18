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

/// TOML extension
pub struct TomlExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for TomlExt {
    type Error = Infallible;
    const NAME: &str = "dolang-toml";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do TOML Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        crate::toml::configure(builder);
        Ok(())
    }
}

extension!(TomlExt);
