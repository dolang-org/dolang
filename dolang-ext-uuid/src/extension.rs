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

use crate::{
    global::{self, Global},
    uuid,
};

/// Uuid extension
pub struct UuidExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for UuidExt {
    type Error = Infallible;
    const NAME: &str = "dolang-uuid";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do UUID Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<global::Tag>(&["uuid"], |reg| {
            let global = Global::new(reg);
            let global = reg.register_state(global);
            uuid::configure_vm(reg, global);
        });
        Ok(())
    }
}

extension!(UuidExt);
