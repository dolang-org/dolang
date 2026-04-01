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

use crate::{container, fs, global::Global, pipe_channel, proc, program, shlex, sys, time};

/// Shell extension
pub struct Shell;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for Shell {
    type Error = Infallible;
    const NAME: &str = "dolang-shell";
    const VERSION: Version = Version {
        major: 0,
        minor: 1,
        patch: 0,
    };
    const DESCRIPTION: &str = "Do Shell Extension";

    fn apply_compiler(&self, compiler: &mut Compiler) -> Result<(), Infallible> {
        sys::configure_compiler(compiler);
        proc::configure_compiler(compiler);
        shlex::configure_compiler(compiler);
        time::configure_compiler(compiler);
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        pipe_channel::install(builder);
        sys::configure_vm(builder, global);
        proc::configure_vm(builder, global);
        program::configure_vm(builder, global);
        fs::configure_vm(builder, global);
        container::configure_vm(builder, global);
        shlex::configure_vm(builder);
        time::configure_vm(builder, global);
        Ok(())
    }
}

extension!(Shell);
