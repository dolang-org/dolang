# dolang-shell Architecture

Reusable library + CLI binary for executing Do programs as scripts or through an
interactive REPL. The package exposes the `dolang` entry point, and custom
binaries can call the stock library entry after linking additional extensions.

The CLI also bundles the standard library into the executable as a ZIP trailer
and falls back to that embedded module set when importing bundled modules.

- **VM setup**: single-threaded tokio runtime, extension discovery/registration,
  custom module importer searching `~/.local/share/dolang/site/`
- **REPL**: `rustyline` for history/editing; `$dynamic$` module persists
  variable bindings across evaluations via a "prelude" injection mechanism
- **Bytecode caching**: `~/.cache/dolang/bytecode/`, keyed by Blake3 hash
  of file path
- **Diagnostics**: `annotate-snippets` for colorized output
