#![deny(warnings)]

mod diagnostic;
mod extension;

pub use diagnostic::{
    ColorMode, print_compile_diag_stderr, print_error_stderr, render_compile_diag,
    render_message_backtrace,
};
