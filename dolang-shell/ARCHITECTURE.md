# dolang-shell Architecture

Stock `dolang` CLI binary. The reusable interpreter logic now lives in
`dolang-shell-main`, the stock bundled Do modules live in
`dolang-shell-modules`, and stock bundled `--main` entrypoints live here.

- **VM setup**: single-threaded tokio runtime, extension discovery/registration,
  stock shell config wiring
- **Bundled entrypoints**: `build.rs` compiles stock script entrypoints such as
  `test` and `dodo`
- **REPL**: `rustyline` for history/editing; `$dynamic$` module persists
  variable bindings across evaluations via a "prelude" injection mechanism
- **Bytecode caching**: `~/.cache/dolang/bytecode/`, keyed by Blake3 hash
  of file path
- **Diagnostics**: `annotate-snippets` for colorized output
