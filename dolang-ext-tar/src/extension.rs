use std::convert::Infallible;

use dolang::{
    compile::Compiler,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

use crate::{global::Global, tar};

pub struct TarExt;

impl Extension for TarExt {
    type Error = Infallible;
    const NAME: &str = "dolang-tar";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Streaming TAR Archive Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Self::Error> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        tar::configure_vm(builder, global);
        Ok(())
    }
}

extension!(TarExt);
