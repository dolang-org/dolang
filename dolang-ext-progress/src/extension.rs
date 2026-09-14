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

use crate::global::{self, Global};

/// Indicatif extension
pub struct IndicatifExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for IndicatifExt {
    type Error = Infallible;
    const NAME: &str = "dolang-progress";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Progress Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        // Strand-local keys can only be reserved on the builder
        let local = builder.local();
        builder.lazy::<global::Tag>(&["progress"], move |reg| {
            let global = Global::new(reg, local);
            let global = reg.register_state(global);
            crate::progress::configure_vm(reg, global);
        });
        Ok(())
    }
}

extension!(IndicatifExt);
