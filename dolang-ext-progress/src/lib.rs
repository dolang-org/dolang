#![deny(warnings)]

mod extension;
mod global;
mod plain;
mod progress;
mod style;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
