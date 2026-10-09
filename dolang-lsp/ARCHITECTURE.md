# dolang-lsp Architecture

LSP server for Do using `tower-lsp-server` and `tokio`. Document state
recompiles on every change to provide diagnostics and semantic tokens.

Compilation runs on a dedicated worker thread, so handlers never block the
event loop. Handlers send the worker each document's text, and answer requests
from the latest stored `Document` without waiting for it. The worker keeps the
`Unit<'static>` it last compiled for each open document, reusing it when the
text and settings are unchanged. It projects each unit into a `Document`
holding the unit (whose source is the document's text), semantic tokens,
references, declarations, symbols and quick fixes. An applier task stores each
`Document`, replacing the old one whole, then publishes its diagnostics.

tower-lsp-server runs handlers concurrently, so edits can reach the worker out
of order. Each piece of work carries a stamp: an epoch, which each open of the
document advances, and the client's version. The worker and the applier drop
anything older than what they already hold. A close also goes through the
worker, so the applier drops the document only after any projection still in
flight.

Configuration files are TOML files (`.dolang-lsp.toml`, searched upward) that
currently provide static prelude imports for workspace-specific names.

`dodo install` uses the newly built `dolang` binary to regenerate typelibs
from bundled-library sources, then sets `DOLANG_LSP_TYPELIB_DIR` for the LSP
build. The build script validates and embeds the directory's `.dolt` files.
`typelib_index::lookup` returns a module's bytes for decoding with
`dolang_compile::typeck::Typelib::read` and adding to the check builder.
Checker integration is separate; the index currently supplies its lookup.
Ordinary builds leave the variable unset and embed an empty index, as with
`DOLANG_LSP_DOC_JSON_DIR` for documentation.
