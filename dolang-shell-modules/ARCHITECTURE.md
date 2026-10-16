# dolang-shell-modules Architecture

Owns the stock Do modules bundled into the shell binary.

- **Source ownership**: `lib/` contains the standard Do modules previously
  embedded by `dolang-shell`.
- **Build-time compilation**: `build.rs` compiles each `.dol` source into
  bytecode and typelibs and generates a public resource table.
- **Shell-core integration**: the crate registers its public `RESOURCES` table
  with `dolang::resources!`. The shell links this crate to include stock
  modules.
- **Checking policy**: `_compile` shares source/typelib discovery and checking
  between boot's `--check` and the `compile` entrypoint. It is imported only
  when checking or inferring a module name for typelib generation.
- **Host source loading**: `_script` shares bytecode detection and caching
  between boot and remote entrypoints. File IO uses `shell.with_host`; its
  `run` executes bytecode after restoring the caller's VFS context.
