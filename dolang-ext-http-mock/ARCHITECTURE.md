# dolang-ext-http-mock Architecture

Binds the `wiremock` crate as the `http.mock` module: `http.mock.Server`
wraps a `wiremock::MockServer`, and `server.mock` registers one or more
`wiremock::Mock`s from a **variadic list of matcher/response dicts** (dash
items), each shaped `{method:, path:, path_regex:, headers:, query:,
body_json:, respond:, expect:, name:}`. Registering several related mocks in
one call (e.g. a specific matcher plus a catch-all fallback on the same path)
is the reason for the variadic shape — it was originally one flat set of
kwargs per call, which forced repeated calls for exactly that pattern.

A `do |handle|` block (or the persistent no-block return value) receives a
**single** `Mock` object (Rust struct `MockObject`, to avoid colliding with
`wiremock::Mock`) covering every dash item registered by that call — the Do
object model doesn't have to mirror the Rust structure 1:1. `MockObject`
internally holds a `Vec<MockEntry>` (one per dash item, each with its own
`Option<MockGuard>`/`expect`/`name`), and `received`/`verify`/`unmount` all
operate across every entry. If exposing individual items separately is ever
needed, `MockObject` could grow an array-like view over its entries (e.g. as
`Rule` objects) without changing this outer shape.

Dict keys are looked up via pre-interned `Sym`s (`global.syms.*`), never raw
`&str` literals — bareword vertical-layout/dict-literal keys (`method:
GET`) compile to `Sym` values, not `Str`, so a `Dict::get(strand, "method",
...)` lookup silently never matches.

`Server` exposes `.url` (a `url.Url`) but not a separate raw-string `.uri` —
`url.Url` coerces to a string when needed, and `.uri` was redundant. Build
request paths with `/`, not string interpolation: `Url`'s string form has a
trailing slash (`http://host:port/`), so `"$(server.url)/get"` produces a
double slash; `(server.url / "/get")` (leading-slash "absolute path" form of
the `/` operator) is the correct way to target a path.

## GC-native lifetime management (no `Rc`/`RefCell`)

`Server` and `Mock` root each other and their auxiliary state directly
through the GC via `Object::SLOTS`, rather than through `Rc<RefCell<_>>`
shared ownership:

- `Server` slot 0 holds a Do array of every mounted `Mock`, scoped or
  persistent. For persistent (no-block) mocks it is this crate's keep-alive
  mechanism, so a script doesn't need to retain a persistent mock's return
  value for it to keep matching requests. `.reset()` unmounts every `Mock` in
  it before calling `MockServer::reset`, since wiremock panics when a
  `MockGuard` for a reset mock is used or dropped. Unmounting removes the
  `Mock` from this array (`remove_identical`, an identity scan since array
  views expose no direct "remove by value").
- `Mock` slot 0 holds a back-reference to its owning `Server`, so unmounting
  can find and remove the `Mock` from slot 0's array above without the
  `Server` needing to know about it in advance.
- `Mock` slot 1 (only mocks with at least one `match:`/`respond:` callback)
  holds the `strand.Strand` handle for that mock's background callback
  strand (see below) — keeping it rooted, and letting `.unmount()`/scoped
  cleanup cancel it via `.cancel()`.

Liveness in all cases is just "reachable from a GC root," the same way
everything else in the VM works; nothing here needs manual reference
counting or interior-mutable cells.

## Callback matchers/responders (`match:`/`respond: do |req| ...`)

`match:`/`respond:` accept a `do |req| ...` closure instead of (for
`match:`) a set of declarative matcher kwargs, or (for `respond:`) a
response dict. `req` is a plain Do dict — `{method:, url:, headers:,
body:}`, the same shape `.received()` reports — built fresh for each
invocation. `match:`'s closure returns a Do truthy value; `respond:`'s
returns a response dict, converted through the same `build_response` used
for the static case.

The core difficulty: wiremock's `Match`/`Respond` traits are synchronous and
`Send + Sync`, called from wiremock's own async request-handling code, while
Do closures are only invokable through the non-`Send`, lifetime-branded VM.
Bridging them needs:

1. **A dedicated single-threaded tokio runtime per `Server`**
   (`spawn_runtime`, on its own OS thread) — `MockServer::start()` needs an
   active tokio reactor to spawn its accept loop, and that reactor can't be
   the VM's own thread (running the VM synchronously from inside a foreign
   async callback isn't safe: nothing guarantees the VM is still alive, or
   that the callback runs on the VM's own thread, when wiremock invokes it).
   Only `MockServer::start()` itself needs to run on this dedicated runtime;
   every other wiremock call the rest of this crate makes runs on the main
   thread as normal `.await`s.
2. **One background strand per `Mock`** (not per entry) that owns a
   `tokio::sync::mpsc::UnboundedReceiver<Dispatch>`, spawned via
   `Strand::spawn_background` (`run_callback_strand`). It loops, receiving
   `Dispatch::{Match, Respond}` messages, looking up the right closure by
   index in a parallel Do array of `{match:, respond:}` records (one entry
   per spec, built by `parse_mock_spec` — nil for specs with no callback),
   invoking it, and replying.
3. **`CallbackMatch`/`CallbackRespond`** — plain `Send + Sync` structs
   holding an entry index and a clone of the `mpsc::UnboundedSender`,
   implementing wiremock's `Match`/`Respond` synchronously: build an owned
   `RequestSnapshot` (`Send + 'static` — a `Request` can't cross the thread
   boundary as-is), send a `Dispatch` with a `oneshot` reply channel, then
   `futures::executor::block_on` the reply. This blocks the *dedicated*
   runtime's OS thread for the duration of one callback invocation, which is
   fine — nothing else on that thread needs to make progress for the reply
   to arrive, since the reply comes from the main thread's background
   strand.
4. If the sender is gone (background strand cancelled/finished) or the
   closure itself raises an error, `respond:` falls back to a `500`
   response carrying the failure message rather than propagating a panic
   across the sync/async boundary; `match:` falls back to non-match.

`Strand::spawn_background`'s interrupt token handling matters here:
`Mock`'s callback strand must run under a *nested* token (see
`InterruptToken::nested` in `dolang-runtime`), never a token shared
verbatim with anything else — otherwise cancelling the callback strand
(e.g. via `.unmount()`) would also cancel whatever else that shared token
was attached to. Likewise, the future driving `f` inside
`spawn_background_raw` must go through `Strand::pin_future_call` so it
actually participates in cancellation; a background strand that ignores
its own interrupt token runs forever and blocks the VM's event loop on
shutdown, waiting for a background task that will never finish.

## Expectation checking (`expect:`)

wiremock's own expectation verification (`Mock::expect()` /
`MockServer::verify()` / `Drop for MockGuard`) is panic-only — there is no
public `Result`/`bool`-returning verify API. Since the project's `dist`
profile builds with `panic = abort`, this extension avoids that path
entirely:

- `Mock::expect()` is never called, so a mock's expectation range stays the
  default `Unbounded` (trivially always-satisfied) on the wiremock side.
- Every mock is registered via `MockServer::register_as_scoped`, even ones
  the Do API presents as "persistent" (no trailing block) — it's a strict
  superset of `register` and, with expectations always `Unbounded`, dropping
  the returned `MockGuard` can never hit the panic branch of its `Drop` impl.
- The `MockGuard` is held in the mock handle's own Rust-side state (never
  relying on Do's GC finalization for cleanup timing) and dropped explicitly:
  at `do`-block exit for scoped mocks, or via `.unmount()` for persistent
  ones.
- `expect:` is checked entirely by this crate, by reading the fully public,
  non-panicking `MockGuard::received_requests()` and comparing its length
  against the declared range, raising an ordinary catchable Do error on
  mismatch (`verify_expect` in `src/mock.rs`).

## Other wiremock panics

The same `panic = abort` concern applies to the rest of wiremock's API:

- The `method` and `path` matcher constructors panic on input they reject,
  so `parse_mock_spec` checks it the same way first and raises
  `ValueError`. Other constructors are reached only with typed or
  pre-validated input (`path_regex` is compiled first).
- `.reset()` unmounts every `Mock` before calling `MockServer::reset` (see
  above), so no `MockGuard` outlives the mocks wiremock reset.
