# dolang-shell-main Architecture

Reusable process lifecycle for `dolang`-style binaries.

- **Startup**: validate UTF-8 argv, build a single-threaded Tokio runtime,
  register linked extensions, redirect stdio, and execute the linked
  `dolang.entrypoint/boot` resource. Boot receives argv[0] as `shell.program`
  and the remaining arguments unchanged as `shell.args`.
- **Lifecycle**: the large-stack thread restores terminal state before returning
  an exit status or replacing the process. Signals cancel the root strand;
  output is flushed before runtime shutdown. Unhandled errors are printed as
  a last resort.
- **Startup policy**: supplied entirely by boot. This crate installs no module
  search paths, bundled-module importer, CLI parser, or bytecode cache.
  Embedders link exactly one boot entrypoint; linked resource table order does
  not define portable duplicate precedence.
- **Resources**: `_shell.resource` exposes linked text or byte resources to Do.
- **Compiler service**: `_shell.compile` compiles supplied source or reads a
  source file, applies explicit prelude and strictness settings, prints
  diagnostics, and returns bytecode. It does no bytecode detection or caching.
- **REPL services**: `_shell.Editor` runs Do-aware editing/history under
  `spawn_blocking`; cancellation waits for readline to return.
  `_shell.render_error` formats runtime errors.
