#![deny(warnings)]

// Needed so we can programmatically import the `json` module
#[cfg(feature = "json")]
extern crate dolang_ext_json;

mod extension;
mod global;
mod mock;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
