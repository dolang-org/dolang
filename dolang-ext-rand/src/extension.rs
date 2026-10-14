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

/// Random number generation extension
pub struct RandExt;

/// Lazy setup tag
struct Tag;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for RandExt {
    type Error = Infallible;
    const NAME: &str = "dolang-rand";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Random Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<Tag>(&["rand"], crate::rand::configure);
        Ok(())
    }
}

extension!(RandExt);
