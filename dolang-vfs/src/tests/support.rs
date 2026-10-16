//! Helpers shared across the tests.

use crate::{
    file::File,
    process::{StdioRecv, StdioSend},
};
use tokio::io::AsyncSeekExt as _;

/// Hands `file` to a child process as an output stream, positioned where the
/// handle itself is.
///
/// [`FileHandle::into_stdio_send`] takes the position explicitly, because
/// anything relaying on someone else's behalf has to plant *their* cursor
/// rather than its own. A caller handing over its own file wants this.
pub(crate) async fn stdio_send(mut file: File) -> StdioSend {
    let offset = file.stream_position().await.unwrap();
    file.into_stdio_send(offset).await.unwrap()
}

/// Hands `file` to a child process as its input stream, positioned where the
/// handle itself is. See [`stdio_send`].
pub(crate) async fn stdio_recv(mut file: File) -> StdioRecv {
    let offset = file.stream_position().await.unwrap();
    file.into_stdio_recv(offset).await.unwrap()
}
