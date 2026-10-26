# dolang-ext-http-mock Architecture

Implements the `http.mock` module: a local HTTP server, served with hyper on
the VM's own thread, whose responses come from mocks registered from Do.
`server.mock` takes a **variadic list of matcher/response dicts** (dash
items), each shaped `{method:, path:, path_regex:, headers:, query:,
body_json:, respond:, expect:, name:}`, so one call can register related
mocks together, such as a specific matcher plus a catch-all fallback on the
same path.

A `do |handle|` block (or the persistent no-block return value) receives a
**single** `Mock` object (Rust struct `MockObject`) covering every dash item
of that call. `MockObject` holds a `Vec<MockEntry>`, one per item, each with
its matchers, static response, `expect`, `name` and hit count; `received`,
`verify` and `unmount` operate across every entry. If exposing items
separately is ever needed, `MockObject` could grow an array-like view over its
entries without changing this outer shape.

Dict keys are looked up via pre-interned `Sym`s (`global.syms.*`), never raw
`&str` literals — bareword vertical-layout/dict-literal keys (`method:
GET`) compile to `Sym` values, not `Str`, so a `Dict::get(strand, "method",
...)` lookup silently never matches.

`Server` exposes `.url` (a `url.Url`). Build request paths with `/`, not
string interpolation: `Url`'s string form has a trailing slash
(`http://host:port/`), so `"$(server.url)/get"` produces a double slash;
`(server.url / "/get")` (the leading-slash "absolute path" form of `/`) is the
correct way to target a path.

## Two halves

Requests cross from hyper to the VM over one channel per server:

- **The server task** (`src/serve.rs`) owns the listener and every
  connection. It never touches the VM.
- **The dispatcher strand** (`run_dispatcher` in `src/mock.rs`) owns all mock
  state, matches requests against it, runs callbacks, and replies.

The service sends a `Dispatch { request, reply }` (an owned
`RequestSnapshot` and a `oneshot` sender) and awaits the reply. Every request
takes this hop, callbacks or not, so mock state lives only in the GC and
mounting, unmounting and `reset` need no native copy to keep in sync.

## Server task

`start_server` binds a `tokio::net::TcpListener` on `127.0.0.1:0` and hands
`serve::serve` to `Vm::spawn_task`. It needs a tokio reactor on the VM thread,
which the shell provides; without one, `Server()` raises `RuntimeError`
rather than panicking in `bind`.

`serve` is a single `'static` future that runs every connection and every
HTTP/2 stream task in its own `FuturesUnordered`:

- Each connection is
    `hyper_util::server::conn::auto::Builder::serve_connection`, so HTTP/1 and
    HTTP/2 (prior knowledge) both work. Its bounds want `S::Future: 'static` but
    not `Send`; the service future owns a channel sender and awaits a oneshot,
    so it qualifies.
- `LocalExec`, the builder's executor, sends HTTP/2 stream futures back into
  the same set over an unbounded channel. Handing them to `Vm::spawn_task`
  instead would panic for a stream that opens while `Builder::enter` is
  shutting down.
- The service reads the whole body before dispatching. A body error fails the
  service, and hyper drops the connection.
- The request URL is `http://localhost` plus the request target when the
  target has no authority. A target that doesn't parse gets 400.
- If the dispatcher is gone, the service answers 500 `http.mock: server
  closed`.

The loop exits when the shutdown oneshot resolves (sent or its sender
dropped), when the dispatcher drops its receiver, or on an accept error. It
then drops every connection, the listener, and the `stopped` sender.

## Dispatcher strand

`start_server` spawns the dispatcher with `Strand::spawn_background`, passing
the `Server` as its argument. Each `Dispatch` runs in a `spawn_scoped` child
pushed into a `FuturesUnordered`, the same pattern as `strand.fork`, so a slow
callback doesn't hold up other requests and a callback can make requests to
its own server.

For each request, `respond`:

1. Creates a `Request` object (annex: `Rc<RequestSnapshot>`) and appends it to
   the server's received array.
2. Walks a copy of the mounted-mocks array, since a callback can unmount mocks
   mid-walk. The first mounted mock with a matching entry wins, in mount
   order.
3. Within a mock, an entry matches when all its static `Matcher`s match and
   then its `match:` callback, if any, returns truthy. A callback error counts
   as no match.
4. On a match, it increments the entry's count, appends the `Request` to the
   mock's received array, and answers with the static response or the
   `respond:` callback's result through `build_response`. A `respond:` error
   gives 500 carrying `http.mock: {err}`.
5. With no match, it answers 404 with an empty body.

Responses carry only the headers `respond:` gives, plus `Content-Type:
application/json` for `json:`.

## Slots and lifetime

`Server` and `Mock` root each other and their state through `Object::SLOTS`,
not `Rc<RefCell<_>>`:

| Object   | Slot | Holds                                                                                 |
| -------- | ---- | ------------------------------------------------------------------------------------- |
| `Server` | 0    | Array of mounted `Mock`s, scoped or persistent                                        |
| `Server` | 1    | The dispatcher's `strand.Strand` handle                                               |
| `Server` | 2    | Array of every received `Request`, in arrival order                                   |
| `Mock`   | 0    | The owning `Server`                                                                   |
| `Mock`   | 1    | Array of `{match:, respond:}` callback records, one per entry (nil without callbacks) |
| `Mock`   | 2    | Array of the `Request`s this mock matched, in arrival order                           |

`Server` slot 0 keeps a persistent mock alive without the script holding its
return value. Unmounting (`.unmount()`, the end of a scoped block, or
`.reset()`) clears `mounted`, empties the mock's received array, and removes
the mock from slot 0 by identity (`remove_identical`). `expect:` is checked
against the entry's count (`verify_expect`); an unmounted mock reports
`<label>: unmounted`. A scoped block verifies and then unmounts at exit,
combining both results.

Shutdown:

- **`close`** (and the end of a `Server do` block) drops the shutdown sender,
  cancels the dispatcher, and awaits `stopped`, so requests fail as soon as it
  returns. Methods afterwards raise `StateError`.
- **An unreachable, unclosed `Server`** and its dispatcher form a garbage
  strand cycle. The collector unwinds the strand, its receiver drops, and the
  server task exits.
- **At VM exit**, the background strand is cancelled with the rest, and the
  server task exits the same way.

If the VM thread is stalled, for example in a tight loop, the mock server
stops answering until it resumes.
