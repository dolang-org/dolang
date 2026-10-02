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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use dolang::compile::{Severity, typeck};

    use super::{Config, StockConfig};

    /// Every bundled typelib, from `dolang`, its extensions and the shell's
    /// modules, reads as the module it is listed as, and they check together.
    #[test]
    fn bundled_typelibs_check_together() {
        let bundled: Vec<_> = dolang::compile::typelibs()
            .chain(dolang_shell_modules::typelibs())
            .collect();
        let mut names = HashSet::new();
        for (name, _) in &bundled {
            assert!(names.insert(*name), "`{name}` is bundled twice");
        }
        for name in ["std", "strand", "math", "proc", "json", "args"] {
            assert!(names.contains(name), "`{name}` is not bundled");
        }
        let typelibs: Vec<_> = bundled
            .iter()
            .map(|(name, bytes)| {
                let typelib = typeck::Typelib::read(bytes).unwrap();
                assert_eq!(typelib.module(), *name);
                typelib
            })
            .collect();
        let mut builder = typeck::Builder::new();
        builder.pipes(("proc", "PipeSender"), ("proc", "PipeReceiver"));
        for typelib in &typelibs {
            builder.typelib(typelib).unwrap();
        }
        let check = builder.check();
        let errors: Vec<_> = check
            .diagnostics()
            .filter(|diag| diag.severity() == Severity::Error)
            .map(|diag| diag.message().to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn stock_config_exposes_known_entrypoints() {
        let config = StockConfig;
        assert!(config.bundled_entrypoint("test").is_some());
        assert!(config.bundled_entrypoint("dodo").is_some());
        assert!(config.bundled_entrypoint("ssh").is_some());
        assert!(config.bundled_entrypoint("fs").is_some());
        assert!(config.bundled_entrypoint("proc").is_some());
    }
}
