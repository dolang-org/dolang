#![deny(warnings)]

pub mod alias;
pub mod hashbrown;
pub mod intern;
pub mod mono;
pub mod pairsort;
pub mod pin;
pub mod ring;
pub mod verified;

/// Returns whether debug diagnostics are enabled.
///
/// Debug diagnostics require both the crate's `debug` feature and the presence of the
/// `DOLANG_DEBUG` environment variable.
#[cfg(feature = "debug")]
#[doc(hidden)]
pub fn debug_enabled() -> bool {
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("DOLANG_DEBUG").is_some())
}

/// Writes a message to standard error when debug diagnostics are enabled.
#[cfg(feature = "debug")]
#[macro_export]
macro_rules! debug_eprintln {
    ($($arg:tt)*) => {
        if $crate::debug_enabled() {
            ::std::eprintln!($($arg)*);
        }
    };
}

/// Discards debug diagnostics when the `debug` feature is disabled.
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! debug_eprintln {
    ($($arg:tt)*) => {};
}
