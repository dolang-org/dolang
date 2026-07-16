# dolang-shell-core Architecture

Reusable interpreter core for `dolang`-style binaries.

- **Startup**: parse CLI args, build a single-threaded Tokio runtime, register
  linked extensions, and run either batch mode or the REPL.
- **Importer layering**: first search explicit `--module-path` roots in order,
  then `site/`, then fall back to embedded resources supplied by the active
  config.
- **Bundled entrypoints**: `-m` / `--main` runs precompiled bundled script
  entrypoints rather than importing modules and calling `main`.
- **Configuration**: the `Config` trait is the non-feature parameterization seam
  for embedded modules, bundled entrypoints, and future bundle-specific startup
  policy.
