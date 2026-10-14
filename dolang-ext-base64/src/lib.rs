#![deny(warnings)]

mod base64;
mod extension;

pub use extension::Base64Ext;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
