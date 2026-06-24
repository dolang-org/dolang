use dolang_shell_core::Config;

include!(concat!(env!("OUT_DIR"), "/bundled_entrypoints.rs"));

pub(crate) struct StockConfig;

impl Config for StockConfig {
    fn bundled_module(&self, name: &str) -> Option<&'static [u8]> {
        dolang_shell_modules::get(name)
    }

    fn bundled_entrypoint(&self, name: &str) -> Option<&'static [u8]> {
        BUNDLED_ENTRYPOINTS
            .iter()
            .find(|(entrypoint, _)| *entrypoint == name)
            .map(|(_, bytes)| *bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, StockConfig};

    #[test]
    fn stock_config_exposes_known_entrypoints() {
        let config = StockConfig;
        assert!(config.bundled_entrypoint("test").is_some());
        assert!(config.bundled_entrypoint("dodo").is_some());
    }
}
