//! Platform backend for the registry extension.
//!
//! Registration itself ([`crate::wire::WinRegExt`]) is unconditional so the
//! extension is remoteable and gives a real, catchable `Unsupported` error
//! on non-Windows peers rather than a "no such extension" routing failure.

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub(crate) use self::stub::handle;
#[cfg(windows)]
pub(crate) use self::windows::handle;
