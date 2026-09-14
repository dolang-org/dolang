use crate::global::{self, Global};
use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::vm::Builder,
};
use std::convert::Infallible;

pub struct WinnetExt;
impl Extension for WinnetExt {
    type Error = Infallible;
    const NAME: &str = "dolang-winnet";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Windows NetAPI Extension";
    const DEPENDS: &'static [&'static str] = &[<dolang_ext_shell::Shell as Extension>::NAME];
    fn apply_compiler(&self, _config: &mut Config) -> Result<(), Self::Error> {
        Ok(())
    }
    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        builder.lazy::<global::Tag>(&["winnet"], |reg| {
            let global = Global::new(reg);
            let global = reg.register_state(global);
            let module = reg.module("winnet");
            let module = crate::user::configure_module(module, global);
            let module = crate::policy::configure_module(module, global);
            let module = crate::group::configure_module(module, global);
            let module = crate::share::configure_module(module, global);
            let module = crate::connection::configure_module(module, global);
            let module = crate::domain::configure_module(module, global);
            crate::machine::configure_module(module, global).commit();
        });
        Ok(())
    }
}
extension!(WinnetExt);
