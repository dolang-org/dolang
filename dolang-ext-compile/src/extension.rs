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

/// Compile extension
pub struct CompileExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for CompileExt {
    type Error = Infallible;
    const NAME: &str = "dolang-compile";
    const VERSION: Version = Version {
        major: 0,
        minor: 1,
        patch: 0,
    };
    const DESCRIPTION: &str = "Do Compile Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = crate::compile::Global::new(builder);
        let global = builder.register_state(global);
        crate::compile::configure(builder, global);
        Ok(())
    }
}

extension!(CompileExt);
