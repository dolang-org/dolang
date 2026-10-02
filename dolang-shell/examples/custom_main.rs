use std::error;

use dolang::{
    compile::Config,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

struct ExampleExtension;

impl Extension for ExampleExtension {
    type Error = std::convert::Infallible;
    const NAME: &str = "custom-main-example";
    const DESCRIPTION: &str = "Example extension linked from a custom shell binary";
    const VERSION: Version = dolang::package_version!();

    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Self::Error> {
        Ok(())
    }

    fn apply_vm<'v>(&self, _builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        Ok(())
    }
}

dolang::extension!(ExampleExtension);

struct ExampleConfig;

impl dolang_shell_main::Config for ExampleConfig {
    fn bundled_module(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::get(name)
    }

    fn bundled_typelib(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::typelib(name)
    }
}

fn main() -> Result<(), Box<dyn error::Error>> {
    std::process::exit(dolang_shell_main::main(ExampleConfig));
}
