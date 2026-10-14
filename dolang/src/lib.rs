#![deny(warnings)]

extern crate self as dolang;

/// Compiler interface.
pub mod compile;
pub mod extension;
pub mod resource;
/// Runtime interface.
pub mod runtime;

/// Resources bundled by this crate.
pub static RESOURCES: &[resource::Resource] = include!(concat!(env!("OUT_DIR"), "/resources.rs"));

resources!(RESOURCES);
