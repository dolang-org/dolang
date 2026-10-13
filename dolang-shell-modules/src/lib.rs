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
