#![deny(warnings)]

#[cfg(not(any(feature = "gzip", feature = "zstd", feature = "xz")))]
compile_error!("enable at least one of the `gzip`, `zstd`, and `xz` features");

mod compress;
mod extension;
mod global;
