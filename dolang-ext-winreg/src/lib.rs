#![deny(warnings)]

mod access_mask;
mod convert;
mod extension;
mod global;
mod key;
mod link_target;
mod subkeys;
mod value_entry;
mod values;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
