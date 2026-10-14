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

/// Windows Service Control Manager extension.
pub struct WinscmExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for WinscmExt {
    type Error = Infallible;
    const NAME: &str = "dolang-winscm";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Windows Service Control Manager Extension";
    const DEPENDS: &'static [&'static str] = &[<dolang_ext_shell::Shell as Extension>::NAME];

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        builder.lazy::<global::Tag>(&["winscm"], |reg| {
            let global = Global::new(reg);
            let global = reg.register_state(global);
            crate::manager::configure_vm(reg, global);
        });
        Ok(())
    }
}

extension!(WinscmExt);
