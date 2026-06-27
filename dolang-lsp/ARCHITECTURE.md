# dolang-lsp Architecture

LSP server for Do using `tower-lsp-server` and `tokio`. Document state
recompiles on every change to provide diagnostics and semantic tokens.
Configuration files are TOML files (`.dolang-lsp.toml`, searched upward) that
currently provide static prelude imports for workspace-specific names.
