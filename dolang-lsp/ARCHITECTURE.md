# dolang-lsp Architecture

LSP server for Do using `tower-lsp` and `tokio`. Document state recompiles on
every change to provide diagnostics and semantic tokens. Configuration files
are Do scripts (`.dolang-lsp.dol`, searched upward) that return settings dicts;
they run on a dedicated thread-local `tokio` runtime (VM is not `Send`/`Sync`)
and communicate with the main LSP server via a message channel.
