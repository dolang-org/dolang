use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
};

use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::vm::{Builder, StateTag},
};

/// Base64 extension
pub struct Base64Ext;

/// Lazy setup tag
struct Tag;

impl StateTag for Tag {
    type State<'v> = ();
}

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for Base64Ext {
    type Error = Infallible;
    const NAME: &str = "dolang-base64";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Base64 Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<Tag>(&["base64"], |reg| {
            reg.register_state::<Tag>(());
            crate::base64::configure(reg);
        });
        Ok(())
    }
}

extension!(Base64Ext);
