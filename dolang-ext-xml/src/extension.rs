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

/// XML extension
pub struct XmlExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for XmlExt {
    type Error = Infallible;
    const NAME: &str = "dolang-xml";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do XML Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<crate::global::Tag>(&["xml"], |reg| {
            let state = crate::global::Global::new(reg);
            let state = reg.register_state(state);
            crate::xml::configure(reg, state);
        });
        Ok(())
    }
}

extension!(XmlExt);
