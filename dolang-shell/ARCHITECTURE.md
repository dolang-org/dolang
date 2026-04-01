# dolang-shell Architecture

Reusable library + CLI binary for executing Do programs as scripts or through an
interactive REPL. The library exposes a stock entry point that custom binaries
can call after linking additional extensions.

- **VM setup**: single-threaded tokio runtime, extension discovery/registration,
  custom module importer searching `~/.local/share/dolang-shell/site/`
- **REPL**: `rustyline` for history/editing; `$dynamic$` module persists
  variable bindings across evaluations via a "prelude" injection mechanism
- **Bytecode caching**: `~/.cache/dolang-shell/bytecode/`, keyed by Blake3 hash
  of file path
- **Diagnostics**: `annotate-snippets` for colorized output
