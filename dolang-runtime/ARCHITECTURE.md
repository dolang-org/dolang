# dolang-runtime Architecture

The `dolang-runtime` crate implements the execution engine for Do programs,
providing bytecode loading, garbage collection, and cooperative concurrency.

## Virtual Machine

The VM is a scoped-duration structure branded with an invariant lifetime `'v`
preventing mixing objects from different VM instances. It manages a
garbage collector, global symbol table, type registry, loaded programs,
etc.

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
object state contains lifetime-transmuted pins or other interior references
that depend on `Object::SLOTS`, that state must live in the main borrow-checked
object so `Object::finalize` and the runtime's finalization path can drop it
before the slots are zeroed during
collection.

Each `Object<'v>` type gets an **automatic type singleton** created at
registration time. Calling `op_type` on an instance returns this singleton,
which can be exposed directly in a module. The singleton is callable (invoking
the type's constructor) and supports subtype checks for exception catching,
etc. `Instance<'v, 'a, T>` and `Type<'v, T>` are the safe handle types for
instances and singletons respectively.

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

## Standard Library

The standard library is mostly implemented as Rust at the moment. It consists
of the core data types (`str`, `dict`, `array`, ...) and a handful of native
modules providing standard functions.
