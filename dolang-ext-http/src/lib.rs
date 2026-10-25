#![deny(warnings)]

// Needed so we can programmatically import the `json` module
#[cfg(feature = "json")]
extern crate dolang_ext_json;

mod body;
mod body_io;
mod extension;
mod global;
mod http;
mod sse;

pub use extension::HttpExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
