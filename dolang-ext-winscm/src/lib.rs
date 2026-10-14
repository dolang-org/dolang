#![deny(warnings)]

mod access_mask;
mod config;
mod convert;
mod extension;
mod flags;
mod global;
mod manager;
mod service;
mod service_info;
mod services;
mod status;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
