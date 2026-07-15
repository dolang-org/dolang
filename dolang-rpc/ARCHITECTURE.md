# dolang-rpc Architecture

`dolang-rpc` is a framed, multiplexed RPC session library.

The crate has two independent facilities:

- Request/response framing and correlation over an arbitrary asynchronous byte
  stream.
- Platform-specific direct transfer of native operating-system handles when a
  session can support it.

An application can use the first facility over separate stdio pipes, a TCP
connection, or a local socket. Direct handles are an optional optimization;
remote protocols use [`Opaque`](#opaque-objects) references instead.

## Protocol Endpoints

A protocol is a marker type. It keeps endpoint type signatures short and is
the future home for protocol-wide static configuration.

```rust
trait Protocol {
    type Request: Serialize + DeserializeOwned;
    type Response: Serialize + DeserializeOwned;
}

struct Vfs;

impl Protocol for Vfs {
    type Request = VfsRequest;
    type Response = VfsResponse;
}
```

The public endpoint types are `Client<P>` and `Server<P>`. Their roles are
semantically significant for call direction and native-handle transfer. A
client sends `P::Request` and receives the correlated `P::Response`; the server
receives requests and sends responses.

```rust
impl<P: Protocol> Client<P> {
    fn call(&self, request: P::Request) -> Call<P>;
}

impl<P: Protocol> Call<P> {
    fn cancel(&mut self);
}

impl<P: Protocol> Future for Call<P> {
    type Output = Result<P::Response, Error>;
    // ...
}
```

Per-variant request/response typing is intentionally deferred. An
application-level macro can later generate a dispatch enum and a trait that
maps an individual request struct to its response type without complicating
the session core.

Callbacks and server-initiated requests are also deferred. They should use an
explicitly separate reverse-direction protocol rather than assuming that
`Response` is a callback request type.

## Server Dispatch

`Server::serve` owns the receive loop and dispatches incoming requests to an
application handler. The session runs each request independently and can
cancel it later. The handler receives an exclusive `CallContext<P>` tied to
that request:

```rust
impl<P: Protocol> Server<P> {
    async fn serve<H>(self, handler: H) -> Result<(), Error>
    where
        H: AsyncFn(&mut CallContext<P>, P::Request) -> P::Response
            + Send
            + Sync
            + 'static;
}
```

The handler is shared between independently dispatched requests and may be
called concurrently. The session owns each returned future, so an unguarded
request remains abortable. Application-level failures belong in `P::Response`.

`CallContext<P>` is not `Server<P>` and is not cloneable. Its exclusive borrow
makes request-scoped state linear. It provides session services appropriate to
request processing, including opaque object registration, acquisition, and
unregistering, and any future callbacks.

## Cancellation Guards

`CallContext::cancel_guard` lets a handler intercept cancellation for a
particular asynchronous operation. It reborrows the context exclusively into
an async closure:

```rust
let result = context
    .cancel_guard(async |context| {
        perform_operation(context).await
    })
    .await;
```

If the request is cancelled while that closure is running, the guard drops the
closure's future and returns `Err(RequestCancelled)` instead. The handler's
future is not dropped. It can use normal Rust error handling to clean up with
the re-acquired `&mut CallContext<P>` and return an ordinary protocol
response, including an application-defined cancellation error.

The mutable reborrow is intentional: a context cannot be used outside the
guard while the guarded operation owns it, and the error path regains the same
linear context. This avoids concurrent session-control operations from one
request.

## Framing And Multiplexing

The session owns frame writes. This prevents concurrent callers from
interleaving frame bytes.

The framing format carries a frame kind, a monotonically increasing `u64`
message ID, and serialized payload bytes. A process will not exhaust a `u64`
counter in practice, so IDs are never reused during a session. When both peers
originate a class of ID, the frame format identifies the origin role, or the
protocol uses independent directional ID spaces.

Initial frame kinds are:

```text
Request  { id, payload }
Response { id, payload }
Error    { id, kind }
Notify   { id, payload }
Cancel   { id }
```

`Request` and `Response` provide ordinary RPC correlation. `Notify` is for
one-way protocol messages. `Error` is a terminal session-level failure for the
correlated request, initially including cancellation. `Cancel` controls a
request already in flight. The exact binary envelope is an implementation
detail, but it must preserve frame boundaries and associate native-handle
attachment serialization state with precisely one frame. Attachment counts are
not part of the envelope: attachment representations in the serialized payload
implicitly determine which handles its deserializer consumes.

Session establishment may later negotiate protocol version, maximum frame
size, and optional capabilities.

## Future Payload Trailers And Fragmentation

Bulk byte data should not have to pass through postcard when its structure is
already described by the request or response. A future message envelope may
therefore carry a raw payload trailer after the postcard payload. Owned
trailers use `Bytes`, allowing existing immutable buffers to enter the writer
without another copy. On receive, the session can read directly into a
preallocated per-message `BytesMut` and expose the resulting `Bytes` through
`CallContext` for requests and a trailer-aware client call API for responses.
Per-message and connection-wide limits must include both postcard and trailer
bytes.

Large messages and trailers may be fragmented and interleaved by message ID.
Each fragment header carries a small flags field including `IS_FIRST` and
`IS_LAST`. The first fragment contains the message kind, postcard payload, and
total trailer length; continuation fragments contain trailer bytes. A fragment
with both flags set is a complete unfragmented message, so ordinary small calls
retain the one-frame fast path. An abort indicator terminates an incomplete
message with an error.

```text
Fragment { id, flags: IS_FIRST | IS_LAST, message }
Fragment { id, flags: IS_FIRST, message, trailer_len }
Fragment { id, flags: 0, trailer_bytes }
Fragment { id, flags: IS_LAST, bytes: [] }
Fragment { id, flags: ABORT, error }
```

The session writer can schedule one bounded fragment from each active message
before checking newly queued messages, preventing a bulk transfer from
blocking small calls and control messages. Abort and cancellation fragments
receive priority. The receiver retains incomplete assemblies by message ID;
`IS_LAST` dispatches a request or completes a response, while `ABORT` discards
an incomplete request or completes an incomplete response with an error.
Unknown, duplicate, and terminally completed fragment sequences are protocol
errors or defined late-message no-ops as appropriate.

Unix messages carrying `SCM_RIGHTS` ancillary data remain unfragmented. The
association between descriptors, stream fragments, and future interleaved
message assemblies is too platform-dependent to make fragmentation a useful
V1 extension. The sender must be able to query whether serialization attached
descriptors and select the atomic path. Such messages remain subject to the
ordinary maximum frame size.

A later optimization may send a trailer borrowed from storage which is not
independently owned or `'static`, such as a Do GC object. The `Call` retains the
Rust borrow, while shared state contains a lifetime-erased slice which the
writer may access only while holding a lock. Dropping the call invalidates the
slice under the same lock before the borrow ends. The writer awaits readiness
without touching the pointer and holds the lock only around each nonblocking
write.

Invalidation can race with a partially transmitted data fragment. Once a
fragment header declares `N` bytes, the sender must finish that fragment's
declared length to preserve stream synchronization. If its borrowed slice is
invalidated, it pads the unsent portion with arbitrary owned bytes and then
sends `ABORT`; the receiver drops all accumulated bytes.

Raw trailers always use a separate zero-length `IS_LAST` fragment, even when
the preceding data fragment contains the end of the trailer. Marking the data
fragment itself `IS_LAST` would commit the message before a borrowed slice was
fully written. With vectored I/O, the writer may attempt the final trailer data
and the owned terminal header in one operation. If the write reaches any part
of the terminal header, all trailer bytes were written first, so the writer can
safely finish that partial header. If the borrowed payload must instead be
aborted before any terminal-header byte was written, the writer completes the
declared data-fragment length with padding and substitutes an `ABORT` fragment.

This is an unsafe implementation technique, not a public promise that
arbitrary non-`Send` or movable storage can be borrowed. The backing storage
must remain stable and safe for immutable cross-thread access until
invalidation completes. Owned `Bytes` trailers are the preferred initial
implementation.

## Cancellation

A client may cancel any of its still-pending request IDs through
`Call::cancel(&mut self)`. The operation is non-consuming and idempotent: it
sends at most one `Cancel { id }`, while the same `Call` future remains
awaitable. Cancellation is advisory. Its result is observed through that future,
not through a separate acknowledgement channel: it resolves as an error just as
a connection failure would. Dropping `Call` sends the same cancellation request
best-effort, but leaves no caller to observe its result.

`Server::serve` tracks each handler task by its request ID. On `Cancel`, it
signals the request context. If a `cancel_guard` is active, that guard drops
its inner future and returns `RequestCancelled` to the handler. The handler
then decides how to clean up and what response to send. If no guard intercepts
the cancellation, the server aborts the handler task and writes
`Error { id, Cancelled }` only after the task has finished, so its future has
been dropped before the caller observes `Err(Error::Cancelled)`.

Cancellation races normally with completion. If the handler completes before
the cancellation takes effect, the request receives its normal `Response`.
If an unguarded cancellation wins, it receives `Error { id, Cancelled }`; if a
guard intercepts it, the handler may instead return its chosen `Response`. A
late or unknown `Cancel` is a silent no-op; it does not create a second
terminal message for the original request.

Cancellation is transport-independent. It applies equally to TCP, stdio, and
local attachment-capable sessions.

## Serialization Context

Direct handles require per-frame state while values are serialized and
deserialized. The crate passes this state explicitly through wrapper
`Serializer` and `Deserializer` implementations and serde seeds. The wrappers
use transactional handle contexts supplied by the transport; application
payload types never access the transport directly.

Serialization obtains the transport's associated `Send` value. Serializing an
`OsHandle` calls the platform-appropriate attachment method, which stages or
copies the native handle and returns its wire representation. Unix descriptor
attachment returns a queue index. Windows handle attachment returns the actual
peer-local `HANDLE` value at pointer width. A self-by-value finishing method
takes the complete header-and-payload buffer and sends it with all staged
handles. Dropping a Unix `Send` before finishing clears its staged descriptors.
Windows server-to-client duplication is immediate. The send frame records each
peer-local result and makes a best-effort attempt to close them if the frame is
dropped during serialization; once transmission starts, delivery is ambiguous
and cleanup is deliberately disarmed.

Unix `Send` retains `BorrowedFd`s for its full lifetime rather than duplicating
them. Requests and responses are therefore moved to the writer task and kept
alive until the consuming send operation completes. The contextual serializer
is the only unsafe bridge from serde's erased raw descriptor representation to
the frame lifetime.

The transport accumulates received native handles internally as reads
complete. The receive loop obtains an associated `RecvFrame` value before
reading a frame and retains it through deserialization. The contextual
deserializer takes handles directly from this value. Handles received early for
a later frame remain in the receiver. On Unix, dropping `RecvFrame` removes the
prefix through the largest descriptor index taken while decoding; any
unconsumed descriptors in that prefix are closed. No attachment count is
needed.

This replaces thread-local queues of sent and received descriptors. Explicit
context makes nested serialization, cancellation, tests, and concurrent
sessions tractable, and it makes the association between a payload and its
attachments unambiguous.

## Direct Native Handles

The type for a directly usable native OS resource is `OsHandle<T>`, whose
default parameter is the platform alias `DefaultHandle` (`OwnedFd` on Unix and
`OwnedHandle` on Windows). The name is deliberately descriptive rather than
capability- or rights-oriented: an open OS handle often has security
significance, but the type represents a local resource, not an application
authorization scheme. It lets platform-neutral protocol definitions use plain
`OsHandle` while code which borrows or wraps a resource can specify another `T`.

`OsHandle<T>` is serializable only on a direct-handle transport. Attempting to
enqueue or dequeue a native handle through the generic byte-stream transport is
API misuse and panics. Applications that need transport-independent resource
access use `Opaque<M>` instead.

On Unix, a local Unix-domain socket transfers descriptors with `SCM_RIGHTS`.
The Unix transport owns an `AsyncFd` for the socket, performs `sendmsg` and
`recvmsg`, and maintains the descriptor queues. The kernel's association
between ancillary data and stream fragments is OS-dependent: once a complete
frame has been received, all of its descriptors are assumed to be available,
while descriptors received early for the next frame remain queued.

Unix descriptor indexes must be honored rather than treated as traversal-order
checks. A serialization format or custom serde implementation may deserialize
handle fields in a different order than it serialized them. The receive state
therefore keeps stable indexed slots, allowing `RecvFrame` to take the
descriptor at the requested index without shifting the remaining indexes.
Dropping the frame removes its consumed prefix; descriptors already received
for a later frame remain queued.

macOS requires extra care: received descriptors cannot be atomically marked
close-on-exec by the kernel. The implementation should coordinate descriptor
receipt with process creation through a process-wide lock and `pthread_atfork`
handlers, setting `FD_CLOEXEC` before receipt leaves the critical section.
This requires process spawning/forking to observe the same discipline.

On Windows, transfer is role-directed and happens synchronously during
serialization or deserialization:

- Client to server: the client writes its local `HANDLE` value; the server
  duplicates it from the client process into the server process while decoding.
- Server to client: the server duplicates its local handle into the client
  process while encoding, then writes the resulting client-local `HANDLE`
  value; the client adopts that value while decoding.

This model supports privilege-separated VFS deployments where the client
cannot open the more privileged server with `PROCESS_DUP_HANDLE`. The server
holds the process-handle rights needed to duplicate in both directions. The
named-pipe endpoint role is independent of the RPC role and is used only to
discover the peer process ID.

Windows named-pipe client construction takes ownership of a trusted peer
process handle. It verifies that the process ID represented by the handle
matches the process ID reported by the named pipe, then retains the handle for
the lifetime of the session. The handle must grant query and synchronization
access so a future shared-memory handle-transfer implementation can fence
cleanup on peer process exit.

The Windows client retains each outbound request value until its correlated
response or error arrives, ensuring any process-local handle values remain
valid while the server decodes them. After a connection failure, those values
remain retained until the client session itself is dropped so a racing server
decode cannot observe a prematurely closed handle.

V1 does not acknowledge server-to-client handle adoption. The send frame
records handles duplicated during serialization and makes a best-effort
attempt to close them in the client process if serialization fails. Once
transmission begins, the client-local handle may remain open until the client
process exits. This bounded leak is safer than remotely closing an ambiguously
delivered handle whose numeric slot may have been reused.

Winsock socket duplication has different mechanics from `DuplicateHandle` and
is out of scope for the first version. The attachment codec should nevertheless
be extensible by attachment kind so it can be added without redefining ordinary
native-handle semantics.

### Future Windows Handle Reclamation

Acknowledging a duplicated Windows handle over the same fallible connection
does not safely establish when the sender may close it. A lost acknowledgement
leaks, while treating a lost acknowledgement as failure can close a numeric
handle slot after the peer adopted it and reused that slot for an unrelated
resource. V1 therefore accepts bounded server-to-client leaks after ambiguous
transmission failures.

A future local transport can close this race with a shared-memory ring of
atomic handle slots. Two sentinel values distinguish empty and reserved slots.
The server reserves a slot, passes the address of that shared slot directly as
`DuplicateHandle`'s output location, and publishes the completed fragment. The
client clears a slot only after successfully adopting its handle. Handle wire
representations may use the slot index, or use the handle value with a slot
lookup.

On connection failure, the client first waits for the server process handle to
be signaled, fencing any in-progress `DuplicateHandle` and shared-memory
writes. It can then close every slot which is neither empty nor reserved and
clear the ring. This is why named-pipe client construction retains an owned
peer process handle with synchronization access. The reserved state is never
treated as an owned handle during cleanup.

No shared ring is needed for client-to-server transfer in the intended
privilege-separated deployment. The privileged server receives the client's
numeric handle value and duplicates it into its own process while decoding;
it does not create an untracked handle in the less-privileged process. Only
the more privileged endpoint is assumed to possess the rights needed to open
the other process for duplication.

### Future Socket Transfer

Sockets should use a distinct `OsSocket` protocol type rather than pretending
all sockets are ordinary `OsHandle`s. On Unix, `OsSocket` can use the existing
`SCM_RIGHTS` descriptor mechanism. Winsock sockets require their bespoke
duplication protocol, such as `WSADuplicateSocket` protocol information and
reconstruction with `WSASocket`; transferring the raw `SOCKET` value or using
`DuplicateHandle` does not preserve the necessary provider state. This remains
deferred until networking itself is virtualized. Unix socket pass-through for
chained local VFS connections continues to use ordinary descriptor transfer.

## Opaque Objects

`Opaque<M>` is a session-scoped reference to an object owned by one endpoint.
It is always serializable: its wire representation is only the owning role and
a never-reused `u64` ID. It can therefore cross local, stdio, and TCP sessions.
On the non-owning endpoint it is a proxy identity used in RPC requests; it does
not expose the underlying object.

`M` is a public marker type shared by the protocol. The concrete server-side
object can remain private, or even belong to a crate which cannot implement a
trait for the public marker because of Rust's orphan rules.

```rust
trait OpaqueResource: Send + Sync + 'static {
    type Marker: ?Sized + 'static;
}

fn register<T: OpaqueResource>(&self, value: T) -> Opaque<T::Marker>;

fn acquire<T>(&self, opaque: Opaque<T::Marker>)
    -> Result<OpaqueGuard<T>, InvalidOpaque>
where
    T: OpaqueResource;
```

`Opaque<T::Marker>` supplies the static protocol-level type. `acquire::<T>`
also checks the registered concrete `TypeId`: several concrete types may
accidentally share a marker, so the associated-type equality alone cannot prove
the downcast is valid.

The owner stores each value as an erased, reference-counted object together
with its concrete `TypeId`. Acquiring an object retains the entry before
returning a typed `OpaqueGuard<T>`. Unregistering removes the table's public
reference. Thus a concurrent acquire either fails with `InvalidOpaque`, or
succeeds and its guard keeps the concrete object alive until the guard drops.

Opaque lifetime is an application convention: the owner explicitly registers
and unregisters objects. Receiving, copying, or dropping an `Opaque<M>` does
not change its lifetime and does not generate a protocol message. A malformed,
stale, or already-unregistered ID is safely rejected by the table lookup and
concrete `TypeId` check with `InvalidOpaque`.

Opaque references are the basis for fully remote file and socket-like APIs:
the owner registers an object, the peer receives `Opaque<FileMarker>`, and
subsequent read/write/close operations are ordinary RPC calls. A protocol may
choose direct `OsHandle<T>` transfer for a local capable session and fall back
to `Opaque<M>` for a remote one.

## Transport Abstraction

A transport connection is split into `transport::Sender` and
`transport::Receiver` halves. The session writer owns the sender and the receive
loop owns the receiver, eliminating session-level transport mutexes and `Arc`
wrappers. A backend may still share one internally synchronized full-duplex
descriptor through an `Arc`, as the Unix `AsyncFd` and Windows named-pipe
implementations do, while a stdio implementation may use unrelated output and
input streams. The sender does not synchronize multiple frame writes; its
session task remains the sole frame writer.

`Sender` exposes an associated transactional `Send` type whose consuming
`finish` method writes the completed frame. `Receiver`
exposes an associated `RecvFrame<'_>` type which performs both byte reads and
native-handle dequeues while preserving one frame's descriptor-index origin.
Closed internal enums can select and delegate to byte-stream, Unix socket,
Windows local, and pipe implementations without making the generic buffer
methods object-safe.

Transport support is summarized below:

| Transport                                         | Framing/RPC | `Opaque` | `OsHandle`            |
| ------------------------------------------------- | ----------- | -------- | --------------------- |
| Separate stdio pipes                              | yes         | yes      | no                    |
| TCP stream                                        | yes         | yes      | no                    |
| Unix-domain socket                                | yes         | yes      | yes, via `SCM_RIGHTS` |
| Windows local transport with peer process handles | yes         | yes      | yes, via duplication  |

Capability negotiation is useful when a transport can vary at runtime. It is
not a substitute for platform checks: the Windows implementation must also
have the peer process handle and the required duplication rights.

## Non-Goals For The Initial Version

- Generated IDL, request enums, and per-request response typing.
- Server callbacks or a bidirectional application RPC model.
- Winsock socket transfer.
- Raw payload trailers and interleaved message fragmentation.
- Shared-memory reclamation of ambiguously transferred Windows handles.
- Exactly-once delivery or distributed ownership certainty after connection
  failure.
- Making direct handles work over remote transports.

The initial implementation includes the session core, explicit serde context,
request/response multiplexing, opaque-object table, Unix descriptor transfer,
and role-specific Windows named-pipe handle transfer.
