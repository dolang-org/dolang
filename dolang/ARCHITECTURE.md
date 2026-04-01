# dolang Architecture

The `dolang` crate is the public API facade for embedding Do in Rust
applications, re-exporting types from the internal compiler and runtime crates.
Extensions implement the `Extension` trait; the `extension!` macro uses `linkme`
for automatic discovery at link time. `CompilerExt` and `VmExt` traits allow
iterating and applying available extensions without manual registration.
