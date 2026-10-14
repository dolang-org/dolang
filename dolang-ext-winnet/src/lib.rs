#![deny(warnings)]
mod connection;
mod domain;
mod extension;
mod global;
mod group;
mod machine;
mod policy;
mod rights;
mod share;
mod user;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
