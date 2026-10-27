#![deny(warnings)]

extern crate self as dolang;

/// Compiler interface.
pub mod compile;
pub mod extension;
pub mod resource;
/// Runtime interface.
pub mod runtime;
