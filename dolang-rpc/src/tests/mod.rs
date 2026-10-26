mod session;
mod unbound;

use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _};

/// One end of [`duplex`].
pub(crate) type Duplex = Compat<tokio::io::DuplexStream>;

/// An in-memory byte stream pair, as `tokio::io::duplex` makes, seen through
/// futures-io.
pub(crate) fn duplex(max_buf_size: usize) -> (Duplex, Duplex) {
    let (a, b) = tokio::io::duplex(max_buf_size);
    (a.compat(), b.compat())
}
