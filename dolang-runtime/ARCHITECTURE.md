# dolang-runtime Architecture

The `dolang-runtime` crate implements the execution engine for Do programs,
providing bytecode loading, garbage collection, and cooperative concurrency.

## Virtual Machine

The VM is a scoped-duration structure branded with an invariant lifetime `'v`
preventing mixing objects from different VM instances. It manages a
garbage collector, global symbol table, type registry, loaded programs,
etc.

Each VM owns a randomly seeded `RandomState` for value hashing. Hashers copy
its state, so they do not borrow the VM. All value hashing uses this state:
dicts and sets reuse cached hashes across containers, and composite values
incorporate element hashes. The VM brand prevents mixing these hashes with
values from another VM.

Registration goes through `Register`, a handle lent out only as
`&mut Register` from a context that may allocate. `Builder` and
`TypeBuilder` dereference to it. Strand-local keys, importers, traps, and lazy
setups are declared on `Builder` alone, because strands and the import path
depend on them being fixed before the VM is entered.

VM state is keyed by a `StateTag`, whose `State<'v>` associated type determines
the stored value and every lookup's result.

A lazy setup (`Builder::lazy`) is keyed by a `StateTag` and owns a set of module
names. It must register state under that tag, using `()` if it has none of its
own. It runs once, with a fresh `Register`, when an import misses a module it
owns or `AllocExt::force` names its tag. Forcing requires an `Alloc` because
setup allocates; `Vm::state` never forces, and panics if state is missing. The
table tracks running setups to report cycles, checks that each registers its
state and declared modules, and rejects modules registered outside their setup.

## Value Representation

Values use a tagged pointer scheme where LSB bits encode type tags. Primitives
(`nil`, `i128` integers, `f64`, `bool`) are immediate when they fit the tagged
encoding; strings, objects, and numeric values that don't fit in the immediate
range are heap-allocated. Storage abstractions
(`Slot`, `Input`, `Output`) prevent external code from holding raw `Value`
instances to uphold GC invariants.

## Garbage Collector

The GC uses reference counting with trial deletion for cycle detection. Objects
are allocated with headers containing a vtable pointer and bookkeeping fields.
The vtable provides methods to visit and clear reachable objects during cycle
collection. Concrete vtables are generally constructed automatically from
a trait implementation (`Collect`).

Cycle collection runs when net allocations reach `max(10000, live)`, where
`live` estimates the cyclic objects surviving the last collection. The
collector counts objects in its initialization pass and subtracts objects it
clears. Miri uses a fixed threshold of 10 to exercise collection frequently.

Native code may hold a GC reference across an await only where the collector
visits it: a `Strand::with_slots` (`with_slots!`) slot, data visited by a
`Strand::with_scan` closure, an error (whose value and backtrace live in a slot
of the strand's `ErrorTable`), or a GC-visited object. Both `with_slots` and
`with_scan` push a `frame::Scan` rooting frame, which `scan_stack` visits
through its closure. The closure holds a shared borrow of what it roots, so
rooted data that changes needs interior mutability (`Cell`, `RefCell`, or
`UnsafeCell`). Outside those, a reference may sit unvisited, such as in a local
on the Rust stack, only while no collection can happen at all; a tracing
collector would miss it. Count every GC allocation, and any code reached with a
`Strand`, as a possible collection, even though collections currently happen
only at explicit safepoints. `Slot::new` is `unsafe` for this reason: creating a
slot over a location claims the collector can see it.

Creating an object moves references from wherever they are rooted into its
payload. To keep them visible across the allocation, allocate first with an
embryo (`gc::Embryo`, through `TypeHandle::embryo` or `tuple::embryo`), then
finish it with the payload. Allocating the embryo counts as a possible
collection; finishing it never allocates, so the references move straight from
their rooted location into the object. An unfinished embryo is not linked,
traced, or counted.

An object holds a sized value, borrow-checked at runtime, followed by its annex.
The annex is either sized or a `gc::Tail`: a fixed prefix followed by a flexible
array whose length the object records. Immutable state lives only in annexes,
which are read without a borrow guard: an immutable object is a `gc::Imm<T>`,
whose value is a unit and whose annex is `T`. `Str`, `Bin`, and `Tuple` objects,
upvar frames, and class instances are likewise unit values whose state is a tail
annex, so each is a single allocation; mutable elements are `ValueCell`s. A
class instance's native super slots are the exception: they live out of line, so
that a native found through one stays reachable without pinning the instance. A
native object keeps its borrow-checked value, which holds no GC references; its
`Object::SLOTS` slots are the tail of its annex. Clearing during cycle
collection skips immutable state: an immutable object can reference only objects
that existed before it, so any cycle through it passes through mutable state
that is cleared.

### Locations and pins

A `&gc::Base` (such as a `&GcObj`), like a `&Value`, witnesses only an object's
_location_: the reference is address-stable, and some scanned owner sits above
it. `Value::case` and `Value::downcast_ref` view an object `Value` as a `&Base`
in place, which works because both are transparent over the same pointer.
Dereferencing an object's _contents_ additionally needs a `gc::Pin`, a witness
that they will not move. Under the current collector nothing moves and every
pin is free; the ways of getting one differ in what they would cost under a
moving collector:

- `Base::pin_access` pins for the lifetime of a `Strand::access` token, inside
  which nothing can await or collect.
- `Base::pin_sync` and `Base::pin` scope a pin to a closure that receives the
  strand back and may allocate or await. A moving collector would push and pop
  an entry on a strand-local pin list, which a stop-the-world collection
  promotes to live pins.
- `PinStr` and `PinBin` own a reference to their object, so each is both a root
  and a dynamic pin. A native object can hold one beside a structure that
  borrows the text, provided the borrower is dropped first.
- `Recv` and `Instance` hold the pin of their receiver. Protocol dispatch mints
  it unchecked; `Cast::enter` and `RecvCast::enter` mint a scoped pin.
- `Base::pin_unchecked` is for low-level code: dispatch glue, finalizers (which
  have no strand), and transient accesses with no possible collection before
  the pin's last use, such as upvar reads and writes. `Base::read` makes a
  transient read safe by taking a function pointer, which can capture nothing
  that reaches a collector.

Anything a GC object owns in a separate heap allocation stays put when the
object moves. `Base::project` reaches such an `OutOfLine` owner (a `Box`,
`alias::Box`, `Vec`, `String` or `'static` `Cow`) through a transient pin and
returns its target for the lifetime of the location, not the pin. The
interpreter reads bytecode and constant tables this way, and objects that are
read through a location often keep their state out of line for the same
reason: `ClassObject`, native type objects and array views box their annexes,
so `Base::annex` and `Base::get` reach them without a pin. Native type
singletons live as long as the VM and are permanently pinned, so a moving
collector must allocate them where they never move.

`gc::GcCell` and `gc::GcSlot` are the typed counterparts of `ValueCell` and
`Slot`, for a mutable typed location that other code can reach, such as a call
frame's upvar chain. `GcCell::as_ref` lends the stored reference as a location,
and writes go through a slot that drops the old reference only after the write.

A moving collector also rewrites pointers underneath shared references, so
`Repr` and `Base` would need to hold their pointers in an `UnsafeCell`. That
makes `Value` and `GcObj` lose `Freeze`, which rules out a `static` value but
not a `const` one.

## Object Protocol

The dynamic type system is organized in three layers, each building on the one
below.

### Layer 1: Raw Protocol (`object/protocol.rs`)

The lowest layer is the internal vtable mechanism. `Protocol<'v>` is an
internal trait that every heap-allocated object type must implement, covering
all dynamic operations (arithmetic, calls, field access, display, etc.). A
blanket function constructs a concrete vtable automatically from any `Protocol`
implementation. `Dispatch<'v, 'a>` is the safe wrapper over raw vtable
calls on objects.

### Layer 2: Native Type Interface (`object/native.rs`)

The middle layer is the public API for implementing native Rust types as Do
objects. The `Object<'v>` trait lets implementors override individual
operations, declare extra immutable per-instance data (an _annex_), and
register named methods and getters/setters via `TypeBuilder`. The GC wrappers
(`ObjectWrap`, `TypeObjWrap`) implement `Protocol<'v>` by delegating to the
`Object<'v>` impl and to the registered handler tables.

Annex data is immutable and accessible without a runtime borrow guard. It is
only suitable for state that does not need GC-time finalization. If native
object state contains interior references that depend on `Object::SLOTS`, that
state must live in the main borrow-checked object so `Object::finalize` and the
runtime's finalization path can drop it before the slots are zeroed during
collection. A reference into a `Str` or `Bin` should depend on a `PinStr` or
`PinBin` held beside it instead.

Each `Object<'v>` type gets an **automatic type singleton** created at
registration time. Calling `op_type` on an instance returns this singleton,
which can be exposed directly in a module. The singleton is callable (invoking
the type's constructor) and supports subtype checks for exception catching,
etc. `Instance<'v, 'a, T>` and `Type<'v, T>` are the safe handle types for
instances and singletons respectively.

Calling `type` on a native singleton returns a metaclass proxy. Its methods are
unbound and take the native type or a subtype's type object first. The proxy
looks up only the type namespace, so it can reach a type method hidden by an
instance method. Its `(get)` and `(set)` accessors likewise address type
properties and dynamic type hooks. Equality and hashing identify the proxied
singleton; the proxy's own type is `Type`.

Rust code can query a value for a Rust interface named by an `InterfaceTag`
(`object/interface.rs`). `Value::query` passes a `Request` to the object's
`op_provide`, which `ObjectWrap` forwards to `Object::provide` and
`ClassInstance` forwards to each native super slot until one answers. A
provider lends the interface from inside a borrow of its state, so the
consumer receives it in a synchronous closure rather than as a return value.
Errors from either side propagate; a query nothing answers fails with an
unsupported error, which consumers with a fallback test for.

The first such interface, `io::ByteIo`, hands an owned `ByteHandle` over
futures-io traits from a native byte source or sink to another extension.
Providers inherit `io.Io` (`io/protocol.rs`), an abstract type like `Iterable`
that declares the stream methods and capability getters. Taking it closes the
provider. A provider's read-ahead travels with the handle as a private
prefix served before the underlying I/O; a seekable handle realigns past it
before seeking or writing.

Do class proxies use the same native type dispatch for inherited members.
Native handlers receive the registered base `Type<T>` handle even when the
explicit receiver is a Do subclass.

### Layer 3: Do Class Registration (`object/class.rs`)

The top layer handles classes defined in Do source code (the `class` keyword).
A `ClassObject` holds the linearized MRO, a unified symbol table of fields and
methods, and references to any native-type supers. Instances store their field
values and lazily-initialized slots for native super objects. Native types can
participate as superclasses of Do-defined classes, with delegation handled
transparently at the protocol layer.

## Interpreter

Executes bytecode instructions. The core interpreter loop and function/method
dispatch operations use Rust `async`/`await`. Futures are pinned in per-strand
arenas when crossing vtable boundaries to erase their concrete types. Function
dispatch builds explicit linked lists of frames in the call stack in order
to allow live backtraces. Error propagation attaches information from these
frames to allow error backtraces.

## Strand Concurrency

Strands are cooperative tasks based on `Future`s which carry cancellation
tokens to allow explicit cancellation to propagate. Cancellation is handled at
the deepest `Future` in the call stack by dropping it and propagating a
cancellation error instead, allowing Do code (or any native functions) to
handle it using ordinary `Result` mechanisms instead of unceremoniously calling
`Drop` impls. Cancellation tokens are hierachical so that cancellation can
propagate to scoped strands (e.g. in pipelines). Only `async`/`await`/`Future`
and the `futures` crate are used in this scheme, so the system is agnostic as
to the overarching runtime (e.g. `tokio`). As with VMs, strand-scoped types
carry an invariant `'s` lifetime brand to prevent e.g. returning an error on a
strand from which it didn't originate (without an explicit re-homing step at
join boundaries).

### Strand Tree

Every scoped strand belongs to the group of its nearest top-level strand (the
main strand or a background strand), linked into the leader's flat
`descendants` ring, which the GC walks without recursion. Leaders link their
otherwise unused `group_link` into the VM's `strands` ring when they first run,
and unlink when their `StrandInner` drops. A scoped strand also points to its
scoped parent, which always outlives it; immediate children are found by
filtering the group ring on that pointer.

Handles reach strands through a shared `StrandCell`: a pointer that the
`StrandInner` clears when it drops, plus an ID that is never reused. A
background strand's `StrandInner` lives by value in its future, so it drops when
the strand exits rather than when its last `Task` handle goes away. The cell is
attached on first poll, once the inner has a stable address; until then the
`Task` handle can't trace the queued `start` value, which is conservative under
refcounting.

Introspection walks these rings and frame chains synchronously and builds Do
values only afterwards, since any suspension could let a strand exit. A
suspended strand's `sp` points at its innermost frame: Do and slot frames push
it for the GC, and async native frames push it so backtraces include the call a
strand is blocked in. The running strand walks from its live `fp` instead.

## Standard Library

The standard library is mostly implemented as Rust at the moment. It consists
of the core data types (`str`, `dict`, `array`, ...) and a handful of native
modules providing standard functions.
