//! Non-Windows stub backend.
//!
//! Returns a real, catchable [`ErrorKind::Unsupported`] for every request
//! rather than leaving the extension unregistered — a caller on a
//! non-Windows peer should see "SCM not supported here", not a
//! `NotFound`-style "unknown extension" error indistinguishable from a typo
//! in the extension name/version.

use dolang_vfs::{Error, ErrorKind, ExtContext};

use crate::wire::{WinScmRequest, WinScmResponse};

pub(crate) async fn handle(
    _ctx: &mut ExtContext<'_>,
    _request: WinScmRequest,
) -> Result<WinScmResponse, Error> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "the Windows Service Control Manager is not available on this platform",
    ))
}
