#![deny(warnings)]

mod extension;
mod glob;
mod global;

pub use extension::GlobExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
