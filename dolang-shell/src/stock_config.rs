use dolang_shell_main::Config;

static BUNDLED_ENTRYPOINTS: &[(&str, &[u8])] =
    include!(concat!(env!("OUT_DIR"), "/bundled_entrypoints.rs"));

pub(crate) struct StockConfig;

impl Config for StockConfig {
    fn bundled_module(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::get(name)
    }

    fn bundled_typelib(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::typelib(name)
    }

    fn bundled_entrypoint(&self, name: &str) -> Option<&'static [u8]> {
        BUNDLED_ENTRYPOINTS
            .iter()
            .find(|(entrypoint, _)| *entrypoint == name)
            .map(|(_, bytes)| *bytes)
    }
}
