//! Extension interface.
//!
//! Allows enumerating and applying extensions from linked crates when configuring a Do compiler or VM.

use std::{error, ptr::NonNull};

#[doc(hidden)]
pub mod __private {
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

use linkme::distributed_slice;

use crate::{compile::Compiler, runtime::vm::Builder};

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

    /// Apply extension to compiler, such as by registering prelude imports.
    fn apply_compiler<'a>(&self, compiler: &mut Compiler<'a>) -> Result<(), Self::Error>;
    /// Apply extension to VM, such as by registering native modules
    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error>;
}

#[doc(hidden)]
pub struct Vtbl {
    name: &'static str,
    description: &'static str,
    version: Version,

    apply_compiler: unsafe fn(this: NonNull<()>, compiler: &mut Compiler) -> Result<(), Error>,
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
                apply_compiler: |this, compiler| unsafe {
                    this.cast::<T>()
                        .as_ref()
                        .apply_compiler(compiler)
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
#[distributed_slice]
pub static EXTENSIONS: [Erased];

/// Register extension.
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
    pub fn apply(&self, compiler: &mut Compiler) -> Result<(), Error> {
        unsafe { (self.vtbl.apply_compiler)(self.ext, compiler) }
    }
}

/// Compiler extension trait.
///
/// Allows iterating extensions to apply to a compiler.
pub trait CompilerExt {
    /// Iterate available extensions in linked crates
    fn extensions(&mut self) -> impl Iterator<Item = CompilerExtension> + 'static;
}

impl<'a> CompilerExt for Compiler<'a> {
    fn extensions(&mut self) -> impl Iterator<Item = CompilerExtension> + 'static {
        EXTENSIONS
            .iter()
            .map(|Erased { vtbl, ext }| CompilerExtension { vtbl, ext: *ext })
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
        EXTENSIONS
            .iter()
            .map(|Erased { vtbl, ext }| VmExtension { vtbl, ext: *ext })
    }
}
