#![deny(warnings)]

// Needed so we can programmatically import the `json` module
#[cfg(feature = "json")]
extern crate dolang_ext_json;

mod body;
mod extension;
mod global;
mod http;
mod sse;

pub use extension::HttpExt;
