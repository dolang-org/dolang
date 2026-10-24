//! Typelibs of the modules the playground provides, for checking code that
//! imports them.

use dolang::resource::{self, Resource};

pub static RESOURCES: &[Resource] = include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);

/// Each module's name and typelib: `std`, `io`, `math` and `strand`, the extensions
/// [`run`](crate::run) applies, and the `test` shim. Extension discovery is
/// unavailable on Wasm, so the extensions are listed here as they are where
/// they are applied.
pub(crate) fn typelibs() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    let extensions = [
        dolang_ext_base64::RESOURCES,
        dolang_ext_compile::RESOURCES,
        dolang_ext_digest::RESOURCES,
        dolang_ext_glob::RESOURCES,
        dolang_ext_http::RESOURCES,
        dolang_ext_load::RESOURCES,
        dolang_ext_rand::RESOURCES,
        dolang_ext_regex::RESOURCES,
        dolang_ext_term::RESOURCES,
        dolang_ext_time::RESOURCES,
        dolang_ext_toml::RESOURCES,
        dolang_ext_url::RESOURCES,
        dolang_ext_uuid::RESOURCES,
        dolang_ext_xml::RESOURCES,
        dolang_ext_json::RESOURCES,
        dolang_ext_yaml::RESOURCES,
    ];
    resource::iter_in(
        [dolang::RESOURCES]
            .into_iter()
            .chain(extensions)
            .chain([RESOURCES]),
    )
    .filter(|resource| resource.namespace == resource::TYPELIB)
    .map(|resource| (resource.name, resource.payload.as_bytes()))
}
