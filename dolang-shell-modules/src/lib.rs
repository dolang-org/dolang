#![deny(warnings)]

include!(concat!(env!("OUT_DIR"), "/bundled_modules.rs"));

pub fn get(name: &str) -> Option<&'static [u8]> {
    BUNDLED_MODULES
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, bytes)| *bytes)
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
