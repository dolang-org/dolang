# Change Log

## [Unreleased]

- Initial release
- Scripts run as tasks that start `dolang` directly, so paths and arguments
  are never shell-interpreted; untitled buffers run from a temporary file
- Run Do Script with Arguments…, `dolang` task definitions, and Send
  Selection/Line to Do Shell
- Removed the unused `dolang.runInTerminal` setting
- Downloaded tools come from the release matching the extension version,
  instead of `releases/latest`, which skipped prereleases; "Re-download Do
  Tools" replaces the cached bundle
- "Restart Do Language Server", automatic restart when `dolang.lsp.path`
  changes, a warning when the server's version differs from the extension's,
  and a language server that fails to start no longer disables the other
  commands
- `dolang.trace.server` and a "Do" output channel for tool resolution and
  downloads
- Limited support in untrusted workspaces: no language server or script
  execution, and the tool path settings are restricted
- Scripts without a `.dol` extension are recognized by a `dolang` shebang
  line
- An icon, a more descriptive name and Marketplace keywords
- Scripts, `dolang` tasks and new interactive shells run with the project's
  module path, as the language server reports it
