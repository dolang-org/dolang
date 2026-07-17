#![deny(warnings)]

mod compile;
mod extension;
#[cfg(feature = "diagnostic-rendering")]
mod render;

#[cfg(feature = "diagnostic-rendering")]
pub use render::{ColorMode, render_compile_diag};
