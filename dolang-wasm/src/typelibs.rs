//! Typelibs of the modules the playground provides, for checking code that
//! imports them.

use dolang::resource;

/// Each module's name and typelib: `std`, `io`, `math` and `strand`, the extensions
/// [`run`](crate::run) applies, and the `test` shim. `dolang-shell-modules`
/// bundles them by the features this crate enables, which match those
/// extensions.
pub(crate) fn typelibs() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    resource::iter_in([dolang_shell_modules::RESOURCES])
        .filter(|resource| resource.namespace == resource::TYPELIB)
        .map(|resource| (resource.name, resource.payload.as_bytes()))
}
