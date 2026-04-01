#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
mod windows;

#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(not(unix))]
pub(crate) use windows::*;
