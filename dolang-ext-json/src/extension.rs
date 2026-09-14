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

/// JSON extension
pub struct JsonExt;

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

impl Extension for JsonExt {
    type Error = Infallible;
    const NAME: &str = "dolang-json";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do JSON Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<Tag>(&["json"], crate::json::configure);
        Ok(())
    }
}

extension!(JsonExt);
