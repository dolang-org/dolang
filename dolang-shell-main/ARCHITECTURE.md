# dolang-shell-main Architecture

Reusable interpreter core for `dolang`-style binaries.

- **Startup**: parse CLI args, build a single-threaded Tokio runtime, register
  linked extensions, and run a script or bundled entrypoint. With no script,
  select `repl` without changing `shell.program` from nil.
- **Importer layering**: first search explicit `--module-path` roots in order,
  then `site/`, then fall back to linked resources in the `dolang.module`
  namespace.
- **Bundled entrypoints**: `-m` / `--main` runs precompiled bundled script
  entrypoints rather than importing modules and calling `main`.
- **Resources**: typelibs, module bytecode, entrypoint bytecode, and assets are
  discovered through `dolang::resource`. Embedders choose resources by linking
  contributing crates. `_shell.resource` exposes their text or bytes to Do.
- **Entrypoint services**: `_shell.compile` compiles supplied source or reads
  a file, retaining native bytecode detection and caching outside REPL mode.
  `_shell.Editor` runs Do-aware line editing under `spawn_blocking`;
  cancellation waits for readline to return. `_shell.repl_options` bridges CLI
  settings and native history directories until startup moves into Do.
  Directory lookups use a direct VFS and the process environment.
