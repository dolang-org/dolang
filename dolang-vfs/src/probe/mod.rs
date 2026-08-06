//! Login environment recovery.
//!
//! A VFS helper started as an SSH command does not see the environment a
//! logged-in session would have. What is missing differs by platform, so each
//! platform recovers it differently:
//!
//! * On Unix the command is `execve`d with no profile having run, so [`import`]
//!   runs the account's login shell and reads back the environment it produces.
//! * On Windows the OpenSSH server does not build a user environment block at
//!   all. It copies `PATH` out of the registry by hand and leaves everything
//!   else pointing at the service account, so [`import`] reads the user
//!   environment out of the registry itself.
//!
//! In both cases the import happens before the VFS starts serving requests, and
//! explicit `--set` and `--unset` operations are applied afterwards so that they
//! keep precedence.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{emit, import};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::import;
