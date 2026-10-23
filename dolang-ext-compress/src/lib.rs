#![deny(warnings)]

#[cfg(not(any(feature = "gzip", feature = "zstd", feature = "xz")))]
compile_error!("enable at least one of the `gzip`, `zstd`, and `xz` features");

mod compress;
mod extension;
mod global;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
