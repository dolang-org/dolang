//! Bundled resources, independent of compiler and VM initialization.
//!
//! Native targets discover tables registered with [`crate::resources!`]. On Wasm,
//! hosts explicitly compose public tables with [`get_in`] and [`iter_in`].

/// Namespace for module typelibs.
pub const TYPELIB: &str = "dolang.typelib";
/// Namespace for module bytecode.
pub const MODULE: &str = "dolang.module";
/// Namespace for entrypoint bytecode.
pub const ENTRYPOINT: &str = "dolang.entrypoint";

/// A resource's contents. Producers establish text's UTF-8 validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    Text(&'static str),
    Bytes(&'static [u8]),
}

impl Payload {
    /// Read either payload variant as bytes without allocating.
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes(bytes) => bytes,
        }
    }
}

/// A named resource contributed by a linked crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    pub namespace: &'static str,
    pub name: &'static str,
    pub payload: Payload,
}

/// Iterate explicit tables in supplied order, retaining duplicate keys.
pub fn iter_in(
    tables: impl IntoIterator<Item = &'static [Resource]>,
) -> impl Iterator<Item = &'static Resource> {
    tables.into_iter().flatten()
}

/// Return the first matching payload in supplied table and resource order.
pub fn get_in(
    tables: impl IntoIterator<Item = &'static [Resource]>,
    namespace: &str,
    name: &str,
) -> Option<Payload> {
    iter_in(tables)
        .find(|resource| resource.namespace == namespace && resource.name == name)
        .map(|resource| resource.payload)
}

#[doc(hidden)]
pub mod __private {
    #[cfg(not(target_family = "wasm"))]
    pub use linkme;
}

#[doc(hidden)]
#[cfg(not(target_family = "wasm"))]
#[linkme::distributed_slice]
pub static TABLES: [&'static [Resource]];

// Keep the PE/COFF section non-empty even without contributing crates. An empty
// table avoids exposing a synthetic resource and null slice markers under Wine.
#[cfg(not(target_family = "wasm"))]
#[linkme::distributed_slice(TABLES)]
static ANCHOR: &[Resource] = &[];

/// Iterate linked resources, retaining duplicate keys.
///
/// Native table order is unspecified; resources within a table keep their order.
/// On Wasm this iterator is empty; use [`iter_in`] with explicit tables instead.
pub fn iter() -> impl Iterator<Item = &'static Resource> {
    #[cfg(not(target_family = "wasm"))]
    let tables = TABLES.iter().copied();
    #[cfg(target_family = "wasm")]
    let tables = std::iter::empty();
    iter_in(tables)
}

/// Return the first matching linked payload, or `None` if absent.
///
/// Native table order is unspecified, so duplicate precedence across tables is
/// not portable. On Wasm this returns `None`; use [`get_in`] instead.
pub fn get(namespace: &str, name: &str) -> Option<Payload> {
    iter()
        .find(|resource| resource.namespace == namespace && resource.name == name)
        .map(|resource| resource.payload)
}

/// Register a static resource table independently of extension initialization.
#[cfg(not(target_family = "wasm"))]
#[macro_export]
macro_rules! resources {
    ($table:expr) => {
        const _: () = {
            #[$crate::resource::__private::linkme::distributed_slice($crate::resource::TABLES)]
            #[linkme(crate = $crate::resource::__private::linkme)]
            static TABLE: &[$crate::resource::Resource] = $table;
        };
    };
}

/// Registration is unavailable on Wasm; hosts explicitly compose public tables.
#[cfg(target_family = "wasm")]
#[macro_export]
macro_rules! resources {
    ($table:expr) => {};
}
