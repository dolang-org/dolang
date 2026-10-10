//! Typelibs of the modules the playground provides, for checking code that
//! imports them.

use dolang::extension::Extension;

static TEST: &[(&str, &[u8])] = dolang::typelibs!();

/// Each module's name and typelib: `std`, `math` and `strand`, the extensions
/// [`run`](crate::run) applies, and the `test` shim. Extension discovery is
/// unavailable on Wasm, so the extensions are listed here as they are where
/// they are applied.
pub(crate) fn typelibs() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    let extensions = [
        dolang_ext_base64::Base64Ext::TYPELIBS,
        dolang_ext_compile::CompileExt::TYPELIBS,
        dolang_ext_digest::DigestExt::TYPELIBS,
        dolang_ext_glob::GlobExt::TYPELIBS,
        dolang_ext_http::HttpExt::TYPELIBS,
        dolang_ext_load::LoadExt::TYPELIBS,
        dolang_ext_rand::RandExt::TYPELIBS,
        dolang_ext_regex::RegexExt::TYPELIBS,
        dolang_ext_term::TermExt::TYPELIBS,
        dolang_ext_time::TimeExt::TYPELIBS,
        dolang_ext_toml::TomlExt::TYPELIBS,
        dolang_ext_url::UrlExt::TYPELIBS,
        dolang_ext_uuid::UuidExt::TYPELIBS,
        dolang_ext_xml::XmlExt::TYPELIBS,
        dolang_ext_json::JsonExt::TYPELIBS,
        dolang_ext_yaml::YamlExt::TYPELIBS,
    ];
    dolang::compile::typelibs().chain(extensions.into_iter().chain([TEST]).flatten().copied())
}
