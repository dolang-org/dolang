# dolang-shell-modules Architecture

Owns the stock Do modules bundled into the shell binary, and the typelibs of
every bundled module, native or not.

- **Source ownership**: `lib/` contains the standard Do modules previously
  embedded by `dolang-shell`. `stub/` declares the native modules of `dolang`
  and the extensions, and `wasm/stub/` the playground's `test` module.
- **Build-time compilation**: `build.rs` compiles `lib/` sources into bytecode
  and typelibs, and stubs into typelibs, and generates a public resource
  table. A table there selects sources by feature: each optional extension has
  one, and a source is bundled only when the features of everything it imports
  are enabled, so a build that leaves an extension out bundles no types for it.
  Extensions that every consumer links have no feature, and the native shell's
  mandatory ones, such as `shell` and `zip`, are bundled unless `wasm` is
  enabled. The selected modules are checked together, and each typelib is
  written from that check. The build fails if the check reports an error or
  warning, a bundled module imports one that isn't bundled, or two sources are
  the same module.
- **Shell-core integration**: the crate registers its public `RESOURCES` table
  with `dolang::resources!`. The shell links this crate to include stock
  modules.
- **Checking policy**: `_compile` shares source/typelib discovery and checking
  between boot's `--check` and the `compile` entrypoint, whose `typelib`
  command checks its sources with the modules they import. It is imported only
  when checking or writing typelibs.
- **Host program loading**: `_script` owns turning a path into a running
  program, for boot and remote entrypoints: source through the bytecode cache,
  bytecode, or a bundle. `load` does all file IO up front under
  `shell.with_host` and returns a function that runs the program in the
  caller's VFS context. It is bootstrap code and isn't shared with tooling.
