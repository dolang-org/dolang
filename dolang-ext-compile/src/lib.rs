#![deny(warnings)]

mod compile;
mod extension;
#[cfg(feature = "diagnostic-rendering")]
mod render;

#[cfg(feature = "diagnostic-rendering")]
pub use render::{ColorMode, render_check_diag, render_compile_diag};

pub use compile::apply_prelude_value;
pub use extension::CompileExt;
