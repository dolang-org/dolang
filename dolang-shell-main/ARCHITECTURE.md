# dolang-shell-main Architecture

Reusable interpreter core for `dolang`-style binaries.

- **Startup**: parse CLI args, build a single-threaded Tokio runtime, register
  linked extensions, and run either batch mode or the REPL.
- **Importer layering**: first search explicit `--module-path` roots in order,
  then `site/`, then fall back to linked resources in the `dolang.module`
  namespace.
- **Bundled entrypoints**: `-m` / `--main` runs precompiled bundled script
  entrypoints rather than importing modules and calling `main`.
- **Resources**: typelibs, module bytecode, entrypoint bytecode, and assets are
  discovered through `dolang::resource`. Embedders choose resources by linking
  contributing crates. `_shell.resource` exposes their text or bytes to Do.
