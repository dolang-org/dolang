#![deny(warnings)]

mod attr;
mod check;
mod comment;
mod document;
mod element;
mod extension;
mod global;
mod xml;

pub use extension::XmlExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
