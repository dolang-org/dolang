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

use crate::{fs, global::Global, pipe_channel, platform, proc, security, shell, shlex, sys, term};

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
    const NAME: &str = "shell";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Shell Extension";
    const DEPENDS: &'static [&'static str] = &[<dolang_ext_time::TimeExt as Extension>::NAME];

    fn apply_compiler(&self, config: &mut Config) -> Result<(), Infallible> {
        shell::configure_compiler(config);
        term::configure_compiler(config);
        security::configure_compiler(config);
        sys::configure_compiler(config);
        proc::configure_compiler(config);
        shlex::configure_compiler(config);
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        pipe_channel::install(builder);
        shell::configure_vm(builder, global);
        term::configure_vm(builder, global);
        security::configure_vm(builder, global);
        sys::configure_vm(builder, global);
        platform::configure_vm(builder, global);
        proc::configure_vm(builder, global);
        fs::configure_vm(builder, global);
        shlex::configure_vm(builder);
        Ok(())
    }
}

extension!(Shell);
