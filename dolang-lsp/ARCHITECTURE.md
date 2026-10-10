# dolang-lsp Architecture

LSP server for Do using `tower-lsp-server` and `tokio`. Document state
recompiles on every change to provide diagnostics and semantic tokens.

Compilation runs on a dedicated worker thread, so handlers never block the
event loop. Handlers send the worker each document's text. Each open or edit
starts a revision, a shared future of its `Document`, which replaces the
document's newest revision unless a newer one arrived first. A request awaits
the newest revision, since its positions refer to the text the client last
sent; ordering requests after edits is left to the client. If the worker passes
over a revision, superseded or failing to compile it, the request answers from
the last stored `Document`. The worker keeps the
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

The worker also type checks each document once its edits have settled. A
document's projection publishes its compile diagnostics at once, and schedules
a check for a short delay later; each newer edit pushes the check back. The
worker waits on its request channel with `recv_timeout` until the earliest
check is due. A check has one source unit, the document's, checked as a script
against the typelibs of the bundled modules it imports, directly or through
other typelibs. An import that isn't bundled is unknown to the checker and
draws no diagnostic. Resolving workspace modules waits on configuration that
says what a source layout means. The check publishes the compile diagnostics
again, merged with the checker's, but only while the stored document still has
the stamp it checked. An unchanged unit, as on a save, keeps its check.

Configuration files are TOML files (`.dolang-lsp.toml`, searched upward) that
currently provide static prelude imports for workspace-specific names.

`dodo install` uses the newly built `dolang` binary to regenerate typelibs
from bundled-library sources, then sets `DOLANG_LSP_TYPELIB_DIR` for the LSP
build. The build script validates and embeds the directory's `.dolt` files.
`typelib_index::lookup` returns a module's bytes for decoding with
`dolang_compile::typeck::Typelib::read` and adding to the check builder.
Ordinary builds leave the variable unset and embed an empty index, as with
`DOLANG_LSP_DOC_JSON_DIR` for documentation, so checks know no bundled module.
`dodo lsp-test` generates both directories as `install` does and runs the
crate's tests against them; tests that need the bundle return early without
it. dolang-lsp is not a default workspace member, so `dodo cargo-test` doesn't
build it a second time without the bundle.
