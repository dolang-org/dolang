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
    const VERSION: Version = Version {
        major: 0,
        minor: 1,
        patch: 0,
    };
    const DESCRIPTION: &str = "Do XML Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let state = crate::global::Global::new(builder);
        let state = builder.register_state(state);
        crate::xml::configure(builder, state);
        Ok(())
    }
}

extension!(XmlExt);
