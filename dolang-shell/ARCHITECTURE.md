# dolang-shell Architecture

Stock `dolang` binary. Process lifecycle lives in `dolang-shell-main`, bundled
modules in `dolang-shell-modules`, and stock entrypoints here.

- **Boot**: `entrypoint/boot.dol` parses CLI arguments and selects script,
  compile, check, or bundled-entrypoint execution. With no target it selects
  `repl`, using the same entrypoint dispatch as `-m`. Help and usage errors go
  to stderr. Non-UTF-8 argv is rejected by the Rust launcher with status 2.
- **Importers**: boot first registers a bundled-resource loader using native
  modules alone. After parsing it installs filesystem search before bundled
  fallback: ordered `--module-path` roots, then the application data directory's
  `site/`, with `foo.dol` before `foo/mod.dol`. Module resolution, source
  compilation, and cache IO run under `shell.with_host`; imported bytecode
  executes in the caller's VFS. Modules receive no CLI prelude.
- **Invocation identity**: boot scopes script arguments and identity through
  `shell.with_override`. Script identities are paths, entrypoint identities
  are strings, and the implicit REPL's identity is nil.
- **REPL**: `repl.dol` owns the loop, bindings, and per-turn `$dynamic$`
  importer. Boot supplies prelude, strictness, and history through the internal
  `_boot_options` import handler. History uses `fs.state_dir`, falling back to
  `fs.data_local_dir` in the host context; `--no-history` skips directory
  lookup and persistence.
- **Bytecode**: `_script` recognizes `.dolc` or the magic header. Its versioned
  Blake3 cache keys include absolute path, mode/module name, strictness, and
  ordered prelude bindings. Source mtime invalidates cache entries; bytecode
  verification errors trigger recompilation. `--no-cache` bypasses the cache.
  The stock shell always links `digest`.
- **Script bundles**: before choosing a script, boot asks `_bundle` for the
  offset of a zip signature at the start or after a `#!` line. A bundle's
  `main.dol[c]` and `lib/` members are read up front, then run by a
  program-bound importer: bundle modules first, memoized and guarded per module
  by a `strand.Resource`, then `load.import`. Modules run without strand I/O.
  Stale `.dolc` is recompiled from its source without a disk cache. `--check`
  and `--compile` reject bundles.
- **Checking**: boot lazily imports `_compile`, shared with `compile check`.
  `--check` retains quiet success and permits partial verification; the tooling
  entrypoint retains summaries and fails partial verification.
- **Build**: `build.rs` compiles stock entrypoints and a separate boot resource
  table for the `custom_main` example. Custom boot tables do not overlap the
  stock table in one binary.
