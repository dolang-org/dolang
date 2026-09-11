//! Extension interface.
//!
//! Allows enumerating and applying extensions from linked crates when configuring a Do compiler or VM.
//!
//! On Wasm, discovery iterators are empty and `extension!` performs no registration.
//! Apply concrete [`Extension`] implementations explicitly when configuring the host.

#[cfg(any(not(target_family = "wasm"), test))]
use std::collections::HashMap;
#[cfg(not(target_family = "wasm"))]
use std::sync::OnceLock;
use std::{error, ptr::NonNull};

#[doc(hidden)]
pub mod __private {
    #[cfg(not(target_family = "wasm"))]
    pub use linkme;

    pub const fn parse_version_component(value: &str) -> u32 {
        let bytes = value.as_bytes();
        assert!(!bytes.is_empty(), "empty package version component");

        let mut result = 0_u32;
        let mut index = 0;
        while index < bytes.len() {
            let digit = bytes[index].wrapping_sub(b'0');
            assert!(digit <= 9, "invalid package version component");
            result = match result.checked_mul(10) {
                Some(result) => result,
                None => panic!("package version component overflow"),
            };
            result = match result.checked_add(digit as u32) {
                Some(result) => result,
                None => panic!("package version component overflow"),
            };
            index += 1;
        }
        result
    }
}

#[cfg(not(target_family = "wasm"))]
use linkme::distributed_slice;

use crate::{compile::Config, runtime::vm::Builder};

/// Version specifier.
///
/// Should follow semver conventions.
#[derive(Copy, Clone)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Construct a version from the current Cargo package version.
#[macro_export]
macro_rules! package_version {
    () => {
        $crate::extension::Version {
            major: $crate::extension::__private::parse_version_component(env!(
                "CARGO_PKG_VERSION_MAJOR"
            )),
            minor: $crate::extension::__private::parse_version_component(env!(
                "CARGO_PKG_VERSION_MINOR"
            )),
            patch: $crate::extension::__private::parse_version_component(env!(
                "CARGO_PKG_VERSION_PATCH"
            )),
        }
    };
}

/// Generic error type returned by extension methods.
pub type Error = Box<dyn error::Error + 'static>;

/// Trait implemented by extensions.
pub trait Extension: Send + Sync + 'static {
    /// Type of error to return to the application
    type Error: error::Error + 'static;
    /// Name of the extension
    const NAME: &str;
    /// Short description of the extension
    const DESCRIPTION: &str;
    /// Extension version
    const VERSION: Version;
    /// Names of extensions that must be applied before this one.
    ///
    /// An extension that reads another's registered state or type objects
    /// while applying itself must name it here, since the order extensions
    /// appear in the link-time slice is otherwise arbitrary. Name the
    /// dependency's [`NAME`](Extension::NAME) constant rather than a string
    /// literal.
    ///
    /// A dependency that isn't linked in, a cycle, or two extensions sharing
    /// a name are all link-time configuration errors and panic when
    /// extensions are enumerated.
    const DEPENDS: &'static [&'static str] = &[];

    /// Apply extension to compiler, such as by registering prelude imports.
    fn apply_compiler<'a>(&self, config: &mut Config<'a>) -> Result<(), Self::Error>;
    /// Apply extension to VM, such as by registering native modules
    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error>;
}

#[doc(hidden)]
pub struct Vtbl {
    name: &'static str,
    description: &'static str,
    version: Version,
    #[cfg(not(target_family = "wasm"))]
    depends: &'static [&'static str],

    apply_compiler: unsafe fn(this: NonNull<()>, config: &mut Config) -> Result<(), Error>,
    apply_vm: for<'v> unsafe fn(this: NonNull<()>, builder: &mut Builder<'v>) -> Result<(), Error>,
}

#[doc(hidden)]
pub struct Erased {
    vtbl: Vtbl,
    ext: NonNull<()>,
}

unsafe impl Send for Erased {}
unsafe impl Sync for Erased {}

#[doc(hidden)]
impl Vtbl {
    pub const fn erase<T: Extension>(ext: &'static T) -> Erased {
        Erased {
            vtbl: Vtbl {
                name: T::NAME,
                description: T::DESCRIPTION,
                version: T::VERSION,
                #[cfg(not(target_family = "wasm"))]
                depends: T::DEPENDS,
                apply_compiler: |this, config| unsafe {
                    this.cast::<T>()
                        .as_ref()
                        .apply_compiler(config)
                        .map_err(|e| e.into())
                },
                apply_vm: |this, builder| unsafe {
                    this.cast::<T>()
                        .as_ref()
                        .apply_vm(builder)
                        .map_err(|e| e.into())
                },
            },
            ext: NonNull::from_ref(ext).cast(),
        }
    }
}

#[doc(hidden)]
#[cfg(not(target_family = "wasm"))]
#[distributed_slice]
pub static EXTENSIONS: [Erased];

// Keep the PE/COFF section non-empty. With no linked extensions, linkme's
// start marker can resolve to null under Wine and constructing the empty slice
// then trips Rust's `slice::from_raw_parts` precondition check.
#[cfg(not(target_family = "wasm"))]
struct Anchor;

#[cfg(not(target_family = "wasm"))]
impl Extension for Anchor {
    type Error = std::convert::Infallible;

    const NAME: &str = "";
    const DESCRIPTION: &str = "";
    const VERSION: Version = Version {
        major: 0,
        minor: 0,
        patch: 0,
    };

    fn apply_compiler(&self, _config: &mut Config<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    fn apply_vm<'v>(&self, _builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
static ANCHOR: Anchor = Anchor;

#[cfg(not(target_family = "wasm"))]
#[distributed_slice(EXTENSIONS)]
static EXTENSIONS_ANCHOR: Erased = Vtbl::erase(&ANCHOR);

/// Orders extensions so every extension follows the ones it depends on.
///
/// Each item is a name and the names it depends on. Ties among extensions
/// that are ready at the same time are broken by original position, so the
/// order only changes when a dependency does.
///
/// # Panics
///
/// Panics on a duplicate name, a dependency that isn't present, or a cycle.
#[cfg(any(not(target_family = "wasm"), test))]
fn order(items: &[(&'static str, &'static [&'static str])]) -> Vec<usize> {
    let mut index_of = HashMap::with_capacity(items.len());
    for (index, (name, _)) in items.iter().enumerate() {
        if let Some(previous) = index_of.insert(*name, index) {
            panic!(
                "extensions {previous} and {index} share the name `{name}`; extension names must \
                 be unique"
            );
        }
    }

    // Edges point from a dependency to the extension that requires it.
    let mut dependents = vec![Vec::new(); items.len()];
    let mut remaining = vec![0_usize; items.len()];
    for (index, (name, depends)) in items.iter().enumerate() {
        for dependency in *depends {
            let Some(&dependency_index) = index_of.get(dependency) else {
                panic!("extension `{name}` depends on `{dependency}`, which is not linked in");
            };
            if dependency_index == index {
                panic!("extension `{name}` depends on itself");
            }
            dependents[dependency_index].push(index);
            remaining[index] += 1;
        }
    }

    let mut ordered = Vec::with_capacity(items.len());
    let mut emitted = vec![false; items.len()];
    while ordered.len() < items.len() {
        // Lowest ready index first; `items.len()` is small enough that
        // scanning beats maintaining a heap, and it keeps ties stable.
        let Some(next) = (0..items.len()).find(|&index| !emitted[index] && remaining[index] == 0)
        else {
            let cycle = items
                .iter()
                .enumerate()
                .filter(|(index, _)| !emitted[*index])
                .map(|(_, (name, _))| *name)
                .collect::<Vec<_>>()
                .join(", ");
            panic!("cycle among extension dependencies: {cycle}");
        };
        emitted[next] = true;
        ordered.push(next);
        for &dependent in &dependents[next] {
            remaining[dependent] -= 1;
        }
    }
    ordered
}

#[cfg(not(target_family = "wasm"))]
fn extensions() -> impl Iterator<Item = &'static Erased> {
    static ORDERED: OnceLock<Vec<&'static Erased>> = OnceLock::new();

    ORDERED
        .get_or_init(|| {
            let linked = EXTENSIONS
                .iter()
                .filter(|extension| !std::ptr::eq(*extension, &EXTENSIONS_ANCHOR))
                .collect::<Vec<_>>();
            let items = linked
                .iter()
                .map(|extension| (extension.vtbl.name, extension.vtbl.depends))
                .collect::<Vec<_>>();
            order(&items)
                .into_iter()
                .map(|index| linked[index])
                .collect()
        })
        .iter()
        .copied()
}

/// Register extension.
#[cfg(not(target_family = "wasm"))]
#[macro_export]
macro_rules! extension {
    ($expr: expr) => {
        #[$crate::extension::__private::linkme::distributed_slice($crate::extension::EXTENSIONS)]
        #[linkme(crate = $crate::extension::__private::linkme)]
        static _EXTENSION: $crate::extension::Erased = $crate::extension::Vtbl::erase(&$expr);
    };
}

/// Compiler extension
pub struct CompilerExtension {
    vtbl: &'static Vtbl,
    ext: NonNull<()>,
}

impl CompilerExtension {
    /// Extension name.
    pub fn name(&self) -> &str {
        self.vtbl.name
    }

    /// Extension short description.
    pub fn description(&self) -> &str {
        self.vtbl.description
    }

    /// Extension version.
    pub fn version(&self) -> Version {
        self.vtbl.version
    }

    /// Apply extension to compiler, such as by registering prelude imports.
    pub fn apply(&self, config: &mut Config) -> Result<(), Error> {
        unsafe { (self.vtbl.apply_compiler)(self.ext, config) }
    }
}

/// Compiler extension trait.
///
/// Allows iterating extensions to apply to a compiler.
pub trait CompilerExt {
    /// Iterate available extensions in linked crates
    fn extensions(&mut self) -> impl Iterator<Item = CompilerExtension> + 'static;
}

impl<'a> CompilerExt for Config<'a> {
    fn extensions(&mut self) -> impl Iterator<Item = CompilerExtension> + 'static {
        extensions().map(|Erased { vtbl, ext }| CompilerExtension { vtbl, ext: *ext })
    }
}

/// VM extension
pub struct VmExtension {
    vtbl: &'static Vtbl,
    ext: NonNull<()>,
}

impl VmExtension {
    /// Extension name
    pub fn name(&self) -> &str {
        self.vtbl.name
    }

    /// Extension short description
    pub fn description(&self) -> &str {
        self.vtbl.description
    }

    /// Extension version
    pub fn version(&self) -> Version {
        self.vtbl.version
    }

    /// Apply extension to VM, such as by registering native modules.
    pub fn apply<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Error> {
        unsafe { (self.vtbl.apply_vm)(self.ext, builder) }
    }
}

/// VM extension trait.
///
/// Allows iterating extension to apply to a VM.
pub trait VmExt {
    /// Iterate extensions available in linked crates.
    fn extensions(&self) -> impl Iterator<Item = VmExtension> + 'static;
}

impl<'a> VmExt for Builder<'a> {
    fn extensions(&self) -> impl Iterator<Item = VmExtension> + 'static {
        extensions().map(|Erased { vtbl, ext }| VmExtension { vtbl, ext: *ext })
    }
}

#[cfg(test)]
mod tests {
    use super::order;

    fn names(items: &[(&'static str, &'static [&'static str])]) -> Vec<&'static str> {
        order(items)
            .into_iter()
            .map(|index| items[index].0)
            .collect()
    }

    #[test]
    fn dependencies_precede_dependents() {
        let items: &[(&str, &[&str])] = &[
            ("winreg", &["shell"]),
            ("shell", &[]),
            ("winscm", &["shell"]),
        ];
        assert_eq!(names(items), ["shell", "winreg", "winscm"]);
    }

    #[test]
    fn transitive_and_diamond_dependencies_are_ordered() {
        let items: &[(&str, &[&str])] = &[
            ("top", &["left", "right"]),
            ("left", &["base"]),
            ("right", &["base"]),
            ("base", &[]),
        ];
        assert_eq!(names(items), ["base", "left", "right", "top"]);
    }

    #[test]
    fn independent_extensions_keep_their_original_order() {
        let items: &[(&str, &[&str])] = &[("c", &[]), ("a", &[]), ("b", &[])];
        assert_eq!(names(items), ["c", "a", "b"]);
    }

    #[test]
    #[should_panic(expected = "which is not linked in")]
    fn missing_dependency_panics() {
        order(&[("winreg", &["shell"])]);
    }

    #[test]
    #[should_panic(expected = "cycle among extension dependencies")]
    fn cycle_panics() {
        order(&[("a", &["b"]), ("b", &["a"])]);
    }

    #[test]
    #[should_panic(expected = "depends on itself")]
    fn self_dependency_panics() {
        order(&[("a", &["a"])]);
    }

    #[test]
    #[should_panic(expected = "share the name")]
    fn duplicate_name_panics() {
        order(&[("a", &[]), ("a", &[])]);
    }
}

#[cfg(target_family = "wasm")]
fn extensions() -> impl Iterator<Item = &'static Erased> {
    std::iter::empty()
}

/// Extension discovery is unavailable on Wasm; apply implementations explicitly.
#[cfg(target_family = "wasm")]
#[macro_export]
macro_rules! extension {
    ($expr:expr) => {};
}
