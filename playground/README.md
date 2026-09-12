# Do playground

A static browser host for Do. The main thread owns CodeMirror; one module worker
loads the Wasm adapter, analyzes source, and executes one fresh VM per run.
Compiler byte offsets are converted to UTF-16 in Rust. Token classification is a
local adaptation of the LSP mapping, without an additional grammar.

Included extensions: `base64`, `compile`, `digest`, `glob`, `json`, `load`,
`rand`, `regex`, `time`, `toml`, `url`, `uuid`, `xml`, and `yaml`. Glob
matching has no filesystem traversal; patch paths are strings on Wasm.
Randomness uses the browser's crypto API, and timers use the host's
`setTimeout`. Dynamic modules can be supplied as source strings through
`compile` and `load`.

## Build

Install Node 24 and the `wasm32-unknown-unknown` Rust target:

```sh
rustup target add wasm32-unknown-unknown
dodo playground
python3 -m http.server 8080 --directory target/playground
```

The build reads the exact `wasm-bindgen` version from `dolang-wasm/Cargo.toml`
and installs the matching CLI into `target/wasm-bindgen/<version>/`. Cargo
reuses that installation on subsequent builds. The build also installs the
locked npm dependencies and writes static assets to
`target/playground/`. `dodo pages` assembles documentation, Rustdoc, and the
playground in `site/`. Generated bindings are written to `playground/pkg/`.

For frontend development, run `dodo playground` once, then
`npm --prefix playground run dev`. Rebuild with `dodo wasm-build` after Rust
changes. No application server, CDN, or additional runtime service is needed.

## Tests

Tests run Chromium headlessly; no display server is needed. On Fedora, install
`nodejs24-full-i18n` alongside Node 24 for its ICU support.

```sh
npm --prefix playground exec -- playwright install chromium
dodo playground-test
```

The task builds production assets, runs adapter tests as Wasm in Node via
`wasm-bindgen-test`, checks Wasm Clippy, and tests the browser UI with
Chromium. Use `dodo wasm-test` to run only the Rust adapter tests. Browser
tests serve the same assets at both `/` and `/repo/playground/` to check
relative worker and Wasm URLs.

Worker requests carry an ID, source version, and complete source. The UI ignores
obsolete analysis and responses from replaced workers. Analysis pauses during
execution. Stop replaces the worker; its pending run and buffered output are
lost. Loading failures are displayed and can be retried by reloading the page.

Only owned strings and serializable records cross the Wasm boundary. VM values,
errors, and GC roots remain inside `Builder::build` and `enter_with_slots`.
