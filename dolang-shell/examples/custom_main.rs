use std::error;

use dolang::{
    compile::Compiler,
    extension::{Extension, Version},
    runtime::vm::Builder,
};

struct ExampleExtension;

impl Extension for ExampleExtension {
    type Error = std::convert::Infallible;
    const NAME: &str = "custom-main-example";
    const DESCRIPTION: &str = "Example extension linked from a custom shell binary";
    const VERSION: Version = Version {
        major: 0,
        minor: 1,
        patch: 0,
    };

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Self::Error> {
        Ok(())
    }

    fn apply_vm<'v>(&self, _builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        Ok(())
    }
}

dolang::extension!(ExampleExtension);

struct ExampleConfig;

impl dolang_shell_core::Config for ExampleConfig {
    fn bundled_module(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::get(name)
    }
}

fn main() -> Result<(), Box<dyn error::Error>> {
    std::process::exit(dolang_shell_core::main(ExampleConfig));
}
