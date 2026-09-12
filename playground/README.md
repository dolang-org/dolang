# Do playground

A static browser host for Do. The main thread owns CodeMirror; one module worker
loads the Wasm adapter, analyzes source, and executes one fresh VM per run.
Compiler byte offsets are converted to UTF-16 in Rust. Token classification is a
local adaptation of the LSP mapping, without an additional grammar.

Included extensions: `base64`, `compile`, `digest`, `glob`, `http`, `json`,
`load`, `rand`, `regex`, `time`, `toml`, `url`, `uuid`, `xml`, and `yaml`. Glob
matching has no filesystem traversal; patch paths are strings on Wasm.
Randomness uses the browser's crypto API, and timers use the host's
`setTimeout`. HTTP requests use the browser's `fetch`, so cross-origin requests
need CORS and request bodies are buffered. Dynamic modules can be supplied as
source strings through `compile` and `load`.

A `test` module provides the assertion functions of the shell's `test` module
(`assert`, `assert_not`, `assert_eq`, `assert_ne`, `assert_throws`, and
`assert_type`), so documentation examples that use them run unchanged.

## Build

Install Node 24 and the `wasm32-unknown-unknown` Rust target:

```sh
rustup target add wasm32-unknown-unknown
dodo playground
python3 -m http.server 8080 --directory target/playground
```

The build reads the locked `wasm-bindgen` version from `Cargo.lock` and installs
the matching CLI into `target/wasm-bindgen/<version>/`. Cargo reuses that
installation on subsequent builds. The build also installs the locked npm
dependencies and writes static assets to `target/playground/`. `dodo pages`
assembles documentation, Rustdoc, and the playground in `site/`. Generated
bindings are written to `playground/pkg/`.

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

## Protocol

Worker requests carry an ID, source version, and complete source. The UI ignores
obsolete analysis and responses from replaced workers. Analysis pauses during
execution. Loading failures are displayed and can be retried by reloading the
page.

### Host

`run(source, host, signal)` executes with a `Host` object and an `AbortSignal`.
`Host` is declared in `dolang-wasm/src/host.rs` and emitted into the generated
TypeScript bindings, so the worker's implementation is type-checked. Host
methods may return a `Promise`, which the VM awaits. Each call also receives an
`AbortSignal` that aborts if the VM abandons the call before it settles, for
example when the run is canceled. Exceptions and rejections become Do runtime
errors.

The worker's host is a table. Capabilities available in a worker are
implemented there directly; capabilities that need the page are forwarded as
messages. `echo` is forwarded.

| Message     | Direction     | Purpose                                                 |
| ----------- | ------------- | ------------------------------------------------------- |
| `call`      | worker → page | Invokes `method` with `args` for run `id` as `callId`   |
| `return`    | page → worker | Settles `callId`, rejecting it if `error` is present    |
| `abortCall` | worker → page | Reports that the VM abandoned `callId`                  |
| `cancel`    | page → worker | Aborts the signal of run `id`                           |

The worker handles `cancel` and `return` on arrival instead of queuing them
behind pending requests.

To add a capability, declare the method in both the extern block and the
TypeScript interface in `host.rs`, bind it to a Do function in `configure`, and
add an entry to the worker's host table. A forwarded method is also added to
`PageMethod` in `src/protocol.ts` and handled by `hostCall` in `src/main.ts`.

### Stopping

Stop sends `cancel`. The VM cancels the run's strand and waits for it to unwind,
so `finally` blocks run and the result reports the `Canceled` error with its
backtrace. Streamed output is kept. Code that never suspends cannot observe
cancellation (#679), so Stop replaces the worker if the run has not finished
after one second.

Apart from the host object and signals, only owned strings and serializable
records cross the Wasm boundary. VM values, errors, and GC roots remain inside
`Builder::build` and `enter_with_slots`.
