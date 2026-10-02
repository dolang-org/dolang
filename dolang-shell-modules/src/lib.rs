#![deny(warnings)]

static BUNDLED_MODULES: &[(&str, &[u8])] =
    include!(concat!(env!("OUT_DIR"), "/bundled_modules.rs"));
static TYPELIBS: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/typelibs.rs"));

pub fn get(name: &str) -> Option<&'static [u8]> {
    BUNDLED_MODULES
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, bytes)| *bytes)
}

/// The typelib of the module named `name`.
pub fn typelib(name: &str) -> Option<&'static [u8]> {
    TYPELIBS
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, bytes)| *bytes)
}

/// Each module's name and typelib.
pub fn typelibs() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    TYPELIBS.iter().copied()
}

pub fn iter() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    BUNDLED_MODULES.iter().copied()
}

#[cfg(test)]
mod tests {
    use super::get;

    #[test]
    fn lookup_exposes_known_modules() {
        assert!(get("dodo").is_some());
        assert!(get("test").is_some());
        assert!(get("docker").is_some());
        assert!(get("transfer").is_some());
    }
}
