#![deny(warnings)]

mod compile;
mod extension;
#[cfg(feature = "diagnostic-rendering")]
mod render;

#[cfg(feature = "diagnostic-rendering")]
pub use render::{ColorMode, render_check_diag, render_compile_diag};

pub use extension::CompileExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
