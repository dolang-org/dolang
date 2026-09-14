use std::convert::Infallible;

use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

use crate::{
    global::{self, Global},
    tar,
};

pub struct TarExt;

impl Extension for TarExt {
    type Error = Infallible;
    const NAME: &str = "dolang-tar";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Streaming TAR Archive Extension";

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Self::Error> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        builder.lazy::<global::Tag>(&["tar"], |reg| {
            let global = Global::new(reg);
            let global = reg.register_state(global);
            tar::configure_vm(reg, global);
        });
        Ok(())
    }
}

extension!(TarExt);
