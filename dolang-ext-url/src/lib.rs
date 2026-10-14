#![deny(warnings)]

mod extension;
mod global;
mod url;

pub use self::url::{create_url, value_to_url};

pub use extension::UrlExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
