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

use crate::{global::Global, sqlite};

/// SQLite extension
pub struct SqliteExt;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for SqliteExt {
    type Error = Infallible;
    const NAME: &str = "dolang-sqlite";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do SQLite Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Infallible> {
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        sqlite::configure_vm(builder, global);
        Ok(())
    }
}

extension!(SqliteExt);
