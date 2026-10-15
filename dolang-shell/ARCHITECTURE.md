# dolang-shell Architecture

Stock `dolang` CLI binary. The reusable interpreter logic now lives in
`dolang-shell-main`, the stock bundled Do modules live in
`dolang-shell-modules`, and stock bundled `--main` entrypoints live here.

- **VM setup**: single-threaded tokio runtime, extension discovery/registration,
  linked resource discovery
- **Bundled entrypoints**: `build.rs` compiles stock script entrypoints such as
  `test`, `dodo`, and `repl`
- **REPL**: bundled `repl.dol` owns the loop and bindings. `_shell.Editor`
  supplies rustyline editing/history on a blocking worker. Each turn runs with
  its own `$dynamic$` struct snapshot and program-specific importer.
  `--no-history` disables history loading and saving.
- **Bytecode caching**: `~/.cache/dolang/bytecode/`, keyed by Blake3 hash
  of file path
- **Diagnostics**: `annotate-snippets` for colorized output
