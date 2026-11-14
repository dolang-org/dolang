# Writing Extensions

Extensions live in `dolang-ext-*` crates and are auto-registered at link time
via `linkme`. An extension implements the `Extension` trait and calls
`extension!` to register itself. A typical extension has these files:

- `extension.rs` — trait impl + `extension!` call
- `global.rs` — global state holding `Type` handles
- One or more implementation files — object types, module functions, etc.

The module's public API and its documentation go in a stub,
`dolang-shell-modules/stub/<module>.dol` (see
[API Reference](DOC_STYLE.md#api-reference)). That crate bundles every typelib,
so it can check each against its dependencies. Give it a feature for the
extension, add a row to the table in its `build.rs` that selects the stub when
the extension's feature and the features of everything the stub imports are
enabled, and forward the feature from each crate that links the extension
(`dolang-shell`, `dolang-wasm`).

## Extension Entry Point

```rust
use dolang::{compile::Compiler, extension, extension::{Extension, Version},
    runtime::vm::Builder};

pub struct MyExt;

impl Extension for MyExt {
    type Error = MyError; // or std::convert::Infallible if you can't fail
    const NAME: &str = "dolang-my-ext";
    const VERSION: Version = Version { major: 0, minor: 1, patch: 0 };
    const DESCRIPTION: &str = "My Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Self::Error> {
        Ok(()) // hook for registering syntax extensions; usually a no-op
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        let global = Global::new(builder);
        let global = builder.register_state::<global::Tag>(global);
        configure_vm(builder, global);
        Ok(())
    }
}

extension!(MyExt); // auto-registers via linkme distributed slice
```

## `Register` vs. `Builder`

`Builder` dereferences to `Register`, which carries the registration API:
`sym`, `register_state`, `register_type`, `build_type`, `module`, and
`module_object`. `TypeBuilder` dereferences to `Register` too. Setup helpers
should take `&mut Register<'v>`; only VM-wide configuration (strand-local keys,
importers, traps) needs `&mut Builder<'v>`. Functions exported for other crates
that only look up state should take `&Vm<'v>`, never a builder.

## Lazy Setup

`Builder::lazy` defers registration until something needs it. The setup runs at
most once: when Do code imports one of the modules it declares, or when Rust
code forces its tag through an `Alloc`. Declare it under the tag naming its
state, so `force_state` can run it and return that state:

```rust
fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
    builder.lazy::<global::Tag>(&["my_ext"], |reg| {
        let global = Global::new(reg);
        let global = reg.register_state::<global::Tag>(global);
        configure_vm(reg, global);
    });
    Ok(())
}
```

- The setup must register state under its tag and exactly the modules it
  declares; missing state or a module mismatch panics. A setup with no state
  of its own names `()` as its state type and registers `()` under its tag.
- Strand-local keys, importers, and traps are `Builder`-only. Reserve them in
  `apply_vm` and move them into the setup.
- `Vm::state` never runs a setup, and panics on state whose setup hasn't run.
  Public functions for other crates that create objects call
  `strand.force_state::<global::Tag>()` (`AllocExt` must be in scope). A
  function that only needs to force can take `&mut dyn Alloc<'v>`, so it works
  from a strand or during registration.
- Code that only recognizes existing objects can use `Vm::try_state`: if it
  returns `None`, the setup hasn't run, so no instance exists.

## Global State (`State<'v, T>`)

`Register::register_state` stores a value for the lifetime of the VM and returns
a `State<'v, K::State<'v>>` handle, where `K: StateTag` names the state type.
`State` is `Copy` and dereferences to the stored value. Use it to hold `Type`
handles and other VM-lifetime data that methods need.

```rust
use dolang::runtime::{Type, vm::{Register, StateTag}};

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub(crate) struct Types<'v> {
    pub(crate) widget: Type<'v, Widget>,
    pub(crate) widget_iter: Type<'v, WidgetIter>,
}

pub(crate) struct Tag;
impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self { types: Types {
            widget: builder.register_type(),
            widget_iter: builder.register_type(),
        }}
    }
}
```

## Modules

`Register::module` creates a native module with exported values and functions.

```rust
pub fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("my_ext")
        .value("Widget", global.types.widget) // export type object
        .function("helper", async move |strand, args, out| {
            unpack!(strand, args => arg);
            // ...
            Ok(())
        })
        .commit();
}
```

## The `Object<'v>` Trait

Native object types implement `Object<'v>`. Key associated items:

| Item              | Purpose                                                 |
| ----------------- | ------------------------------------------------------- |
| `NAME` / `MODULE` | Display name and module path (for error messages)       |
| `SLOTS`           | Number of GC-visible slots (default 0). See below.      |
| `type Annex`      | **Immutable** per-instance data; no borrow check needed |
| `type Type`       | **Mutable** data on the type singleton (usually `()`)   |
| `type TypeAnnex`  | **Immutable** data on the type singleton (usually `()`) |

`Annex` is stored alongside the GC object and accessible without a runtime
borrow check via `Instance::annex()`. Use it for data that never changes after
construction (e.g. a `State` handle, a compiled regex). Mutable per-instance
data goes in the struct itself, accessed through `Instance::borrow_mut()` which
performs a runtime borrow check.

Key methods (all have default no-op impls):

```rust
impl<'v> Object<'v> for Widget {
    const NAME: &'v str = "Widget";
    const MODULE: &'v str = "my_ext";
    type Annex = WidgetAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    // Called at registration time to add methods, getters, supertypes
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("foo", async move |this, strand, args, out| { ... })
            .get("bar", |this, strand, out| { ... })
    }

    // Iterator protocol: return self or an iterator
    async fn input<'a, 's>(this: Instance<'v, 'a, Self>, strand, out) -> Result<'v, 's, ()>;
    // Yield next item; return Ok(true) if yielded, Ok(false) if exhausted
    async fn next<'a, 's>(this: Instance<'v, 'a, Self>, strand, out) -> Result<'v, 's, bool>;
    // Destructuring support
    async fn unpack<'a, 's>(this, strand, unpack: Unpack<'v, 'a>) -> Result<'v, 's, ()>;
    // Display/debug for string conversion
    fn display<'a, 's>(this, strand, w: &mut dyn fmt::Write) -> Result<'v, 's, ()>;
}
```

Comparison methods (`eq`, `lt`, and so on) must decline an operand of a type
they don't compare with by returning `Err(Error::unsupported(strand, "`==`"))`
(with the matching operator), never `Ok(false)`. Declining lets the right
operand's reflected method (`(req)`, `(rne)`) run; if it declines too, `==`
falls back to strict equality, so two unrelated types still compare unequal.

## `TypeBuilder` — Registering Methods and Properties

`TypeBuilder` is the API used inside `Object::build` to register methods,
getters, setters, and supertypes.

```rust
fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
    let some_sym = builder.sym("some_key"); // register a symbol (see below)
    builder
        // Instance methods
        .method("name", async move |this, strand, args, out| { ... })
        .method_with_slots("name", async move |this, strand, args, out, [s0, s1]| { ... })
        // Getters / setters
        .get("prop", |this, strand, out| { ... })
        .set("prop", |this, strand, value| { ... })
        // Type-level methods (on the type object, not instances)
        .type_method("class_method", async move |ty, strand, args, out| { ... })
        // Supertypes (e.g. Iter for iterator types)
        .supertype(TypeObject::Iter)
}
```

**Method signatures** (types are usually inferred):

- **Method**: `async |this: Instance<'v, 'b, T>, strand: &mut Strand<'v, 's>,
  args: Args<'v, 'b>, out: Slot<'v, 'b>| -> Result<'v, 's, ()>`
- **Method with scratch slots**: same but adds `[Slot<'v,'b>; N]` at the end.
  Scratch slots are GC-rooted temporaries for intermediate values.
- **Getter**: `|this: Instance<'v,'b,T>, strand: &mut Strand<'v,'s>,
  out: Slot<'v,'b>| -> Result<'v,'s,()>` (sync, not async)

## Symbol Registration

Keyword argument names and any other interned symbols must be registered with
`Register::sym` (or `TypeBuilder::sym`, which derefs to `Register`). Capture the
returned `Sym` in a closure — symbols are `Copy`.

```rust
fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
    let limit = builder.sym("limit");
    builder
        .method("split", async move |this, strand, args, out| {
            unpack!(strand, args => haystack, ?:limit);
            // limit: Option<Slot>, None if not passed. It shadows the Sym
            // inside the closure, which is fine unless the Sym is needed later.
            ...
        })
}
```

Bareword Do-side dict/vertical-layout keys (`method: GET`, unquoted identifier
keys generally) compile to `Sym` values, not `Str` — even though they look
like plain text. Looking one up from Rust with a raw `&str` via
`Dict::get(strand, "method", ...)` silently returns not-found, since `Str`
and `Sym` are distinct key types. Always look up a known bareword key with a
pre-interned `Sym` (`builder.sym("method")`), never a raw `&str`. Quoted keys
(`"x-custom": v`) produce real `Str` keys, so a dict built from a mix of
bareword and quoted keys may have mixed `Str`/`Sym` key types — this only
matters for `Dict::get()` with a fixed expected key, not for generic
iteration (`Dict::pairs()` + `.to_string(strand)` stringifies either kind the
same way).

## Argument Unpacking (`unpack!`)

The `unpack!` statement binds `Args` to locals, using a parameter list modeled
on Do's, and propagates unpacking errors with `?`.

```rust
// 2 required positionals, 1 optional; c: Option<Slot>
unpack!(strand, args => a, b, ?c);

// Key arguments: `:limit` binds the argument keyed by the in-scope Sym `limit`
// as `limit`; `key: x` binds it as `x`. A key that isn't a bare identifier goes
// in parentheses. `?` makes a parameter optional.
unpack!(strand, args => haystack, ?:limit, ?(global.syms.mode): mode);

// Rests come last: `...rest` (Args), `*rest` (PosArgs) or `**rest` (KeyArgs)
unpack!(strand, args => program, *rest);
```

## Creating Instances

Use `Type::create` (when `Annex: Default`) or `Type::create_with_annex`:

```rust
// In a method or module function:
global.types.widget.create_with_annex(
    strand,
    Widget { /* mutable state */ },
    WidgetAnnex { global, /* immutable state */ },
    &mut out, // Slot to place the new object into
);
```

After creation, you can downcast a `Slot`/`Value` back to an `Instance`:

```rust
let instance = global.types.widget.downcast(&out).unwrap();
let borrow = instance.borrow(strand)?;      // Ref<Widget> — shared
let mut borrow = instance.borrow_mut(strand)?; // Mut<Widget> — exclusive
let annex = instance.annex();                // &WidgetAnnex — no borrow check
```

### Built-in Values

`Output::set` accepts an `Input`: a Rust value with a direct Do counterpart
(numbers, `&str`, `&[u8]`, `Sym`, `ByteHandle`), a reference to a rooted value
(`&Value`, `Slot`, `Root`, `Instance`), or a fixed singleton (`Nil`,
`TypeObject`, `Singleton`). New compound values are built with methods that
write into an output:

```rust
strand.new_array(&mut out);                 // also new_dict, new_set (AllocExt)
strand.intern_sym(name, &mut out);          // AllocExt
strand.array_view(this, Entries, &mut out); // also dict_view (AllocExt)
strand.tuple([&key, &value], &mut out);     // Strand: roots each item as taken
strand.range(0i64, len, &mut out);          // Strand
```

`AllocExt` methods need only an `Alloc` (a `Register` or `Builder` too);
`tuple` and `range` need a `Strand`. To pass a built value as a call argument,
build it into a slot from `with_slots!` first.

### Scoped Helpers

In async code, use the scoped macros. Each one runs its body inline, without a
closure or an extra future, and ends the scope when the body finishes or the
enclosing future is dropped:

```rust
with_slots!(strand, |mut key, mut value| { ... })
with_instance!(strand, cast, |this| { ... })
with_interrupt_mask!(strand, InterruptMask::all(), { ... })
redirect!(strand, output: &sink, { ... })
with_str!(strand, text, |s| { ... })  // also with_slice!
```

Because the body is inline, `?` and `return` in it leave the enclosing
function. If the code holds or inspects the body's result rather than
propagating it, wrap the macro in `async { ... }.await`, so that `?` in the body
ends only the body.

## Flags Types

Expose a bitset by implementing `FlagLike` and registering it with
`FlagLikeExt::register_type` (or `build_type` to add supertypes). Construction
from symbols, set operators, `contains`, and iteration come with it. Don't
write an `int` getter or a `from_int`-style type method:

- `const INT: bool = true` with `to_i128`/`from_i128` adds the `int` getter
  and `T(n)`. `from_i128` returns `None` (a `ValueError`) for an out-of-range
  value or a value with bits the type rejects; whether to keep unnamed bits is
  the type's choice.
- `const COERCE: FlagCoercion` lets `T(other)` accept a value of a nominal
  supertype, subtype, or both. When both types are `INT`, the value converts
  through its integer form. Otherwise it converts by symbol.

`FlagsTypeExt::coerce` converts an API argument exactly as the
single-argument constructor does. Keep the stub's `(call)` overloads (`Int`,
the related mask types) and the argument `Spec` aliases in step.

## Interface Queries

Native code can ask a value for a Rust interface without downcasting to
another extension's type. An `InterfaceTag` names the interface, usually a
trait object shared by unrelated providers. The runtime defines `io::ByteIo`
for byte I/O handles; an extension can define its own the same way:

```rust
pub struct ByteIo;

impl InterfaceTag for ByteIo {
    type Interface<'v, 'a> = dyn IntoByteIo<'v> + 'a;
}
```

A provider implements `Object::provide`. `Request` dereferences to the strand,
so the provider borrows its state with it and lends the interface from inside
the borrow. Test the tag first, so an unrelated query never borrows:

```rust
fn provide<'a, 's>(
    this: Instance<'v, 'a, Self>,
    request: &mut Request<'v, 's, '_>,
) -> Result<'v, 's, ()> {
    if request.wants::<ByteIo>() {
        let mut state = this.borrow_mut(request)?;
        if state.is_closed() {
            return Err(Error::state_error(request, "widget is closed"));
        }
        request.provide::<ByteIo>(&mut *state)?;
    }
    Ok(())
}
```

A consumer calls `Value::query`, which lends the interface to a synchronous
closure for the duration of the provider's borrow. A query on a Do subclass
reaches its native supers. Errors from the provider (such as a conflicting
borrow) or the closure propagate. When nothing provides the interface, the
query fails with `ErrorKind::Unsupported`, which a consumer with a slow path
tests for:

```rust
let res = value.query::<ByteIo, _>(strand, |strand, io| {
    if !io.caps(strand)?.contains(IoCaps::READ | IoCaps::SEEK) {
        return Err(Error::value(strand, "expected a seekable reader"));
    }
    io.get(strand)
});
match res {
    Ok(handle) => { /* fast path: handle.into_read_seek() */ }
    Err(e) if e.kind() == ErrorKind::Unsupported => { /* slow path */ }
    Err(e) => return Err(e),
}
```

### Byte I/O Handles

`IntoByteIo::get` lends the provider's stream as an owned `ByteHandle` over
futures-io traits. The handle shares the stream rather than taking it: the
provider stays usable, and closing the provider ends the handle too. The
handle's own `close` only flushes, so a consumer closes and drops what it gets
whether it opened the stream from a path or was lent it. A consumer gets
through `io::get` rather than a raw query. It passes the provider's `caps` to a
check that can refuse it, then gets. It fails with `ErrorKind::Unsupported` when
nothing provides `ByteIo`:

```rust
let handle = io::get(strand, &value, |strand, caps| {
    if !caps.contains(IoCaps::READ | IoCaps::SEEK) {
        return Err(Error::value(strand, "expected a seekable reader"));
    }
    Ok(())
})?;
```

`ByteHandle::into_read` and its siblings narrow the handle to the traits the
consumer needs, handing it back on a mismatch. `io::error` maps an `io::Error`
to the closest runtime error for code without a richer mapping.

A provider builds the handle with `ByteHandle::with_caps`, or the constructor
matching its capabilities (`ByteHandle::read_seek` and so on). How it shares
the stream depends on what it holds:

- A provider with no buffer of its own, whose stream clones cheaply, such as
  `fs.RawFile` over a `vfs::File`, hands out a clone.
- A provider with a buffer or stack, such as `fs.File`, a `compress` stream or
  an `io.Buffered`, keeps it in `io::Shared` and hands out a clone. Each
  operation on a clone holds the stream until it completes, so consumers and
  the provider see one position and one buffer. `Shared::close` ends the
  stream for every clone and returns it, for the provider to finish or drop.
  `Shared::try_lock` holds it without waiting through a shared reference, for
  a method that runs under a shared borrow and settles the buffer only when
  that is cheap.

`ByteHandle::with_prefix` sets bytes the handle serves before the stream's own,
for a provider whose read-ahead the stream can't take back; a seekable handle
realigns past it before its first seek or write.

The lent interface is `dyn IntoByteIo<'v> + 'a`, where `'a` is the lend, so an
adapter can borrow the provider's state or receiver for the query. Each method
takes the querying strand, and `caps` and
`get` can raise. A provider that lends from its borrowed state still holds the
borrow, so it must not run code that could borrow the same object; native
providers ignore the strand.

A provider type also inherits `io.Io` at runtime: call
`.supertype(TypeObject::Io)` in `build`, and define getters for the capabilities
`caps` reports, as `false` once the provider is closed. A capability it
does not define reads as `false` through `Io`. The type defines its own `read`,
`write`, `seek` and `close`, and one it lacks raises a `FieldError` through
`Io`. The helpers (`read_all`, `write_all`, `copy_to`, `chunks`, and
`(sink)`/`(put)` when the type defines none) come from `Io` and call those
methods, so the type needs no code for them. `Io` is a `Sink`, so the type
doesn't add `.supertype(TypeObject::Sink)` either. In the stub, the provider
class names `Io` as a supertype (`import io: - Io`, then `class File: ... Io`),
and a consumer that only accepts one imports it type-only (`- @Io`) for its
signature. `fs.File` (`dolang-ext-shell/src/fs/file.rs`) is a provider, and
`zip.open` (`dolang-ext-zip/src/zip.rs`) is a consumer that also accepts a path
by opening it and building the handle itself. The `compress` streams
(`dolang-ext-compress/src/compress.rs`) are both: each gets a handle and
provides its own, so they chain.

An extension that only hands a native stream to Do, such as a response body,
needs no provider type: setting a `ByteHandle` as an output
(`Output::set(strand, out, handle)`) wraps it in a runtime `io.Io` that owns
it. Its `read` serves the prefix and then polls the handle once per call, its
`close` closes the handle, and it provides `ByteIo` by lending clones.

A Do subclass of `io.Io` provides `ByteIo` too. Its handle sends requests to
strands that call the stream's `read`, `write` and `seek`, never its `close`, so
a consumer must poll the handle from the VM's event loop, as an `async` native
function awaiting it does, never with `block_on` or from another thread. An
exception from one of those methods
reaches the handle as an `io::Error` with the exception's message, which the
consumer maps as it maps any other.

A native type that inherits `io.Io` but answers no `ByteIo` in `provide` is
served the same way, through its own Do methods. `io.Concat` relies on this.
To stream a Do iterable into a native consumer, such as a request body, call
`io::concat(strand, &iterable, end)`. It returns a read handle over the items'
bytes, converted as `io.Concat` converts them, without the caller holding a
slot.

## Raising Errors

Pick the class by asking whether a type checker could have ruled the failure
out (the hierarchy is in
[Error Handling](../docs/language/error-handling.md#error-types)):

- **Yes**: `Error::type_error`, `Error::value`, or a shape error
  (`Error::missing_key` and friends). These are bugs in the calling script and
  are rarely caught.
- **No**: a class callers can catch. Use `Error::data` (or a module subclass
  of `std.DataError`) when a bad external document or stream caused it, and
  otherwise a module subclass of `std.RuntimeError`.

Classify by what the function is for: bad contents in what it parses or
decodes are data errors, and bad options or settings that control how it works
are value errors. A foreign library's error caused by the caller's own
arguments, such as a builder or parse error from an option, raises a std class
rather than the module's error class.

Convert foreign errors with a method that names the class:

- `into_do_io` for an `io::Error`, which keeps its `sys` class.
- `into_sys` (`dolang_ext_shell::ErrorExt` and `ResultExt`) in crates that
  depend on the shell extension. It also takes a `VfsError`. For a condition
  with no OS error behind it, such as a missing archive entry, build an
  `io::Error` of the matching `ErrorKind` to raise the `sys` class.
- `into_do_decode` for an `io::Error` from a decoder library that reports
  malformed input as `ErrorKind::Other` or truncated input as
  `UnexpectedEof`, as the tar crate does. Those become `DataError`s, and other
  errors map as with `into_do_io`.
- `into_do_value` / `into_do_data` for other errors, keeping only the message.
- A conversion into the module's error class when the error carries structure
  callers need, such as a line and column.

A decoder whose validators are shared with constructors can map the whole
decode result through `Error::value_into_data`, so a check that raises
`ValueError` for a constructor argument raises `DataError` when it rejects a
document.

Use `Error::runtime` when an untyped error is really intended.

A `kind` field that distinguishes cases of one error class takes uppercase
symbols, such as `:SYNTAX:` or `:CYCLE:`. Use it when callers have no real
reason to catch some cases but not others. When they do, use subclasses that
share one annex; splitting a kind out into a subclass later stays mostly
backward compatible. A subclass's `kind` can only narrow its base's, so a base
class with a `kind` must declare the union of all its subclasses' kinds.

A `spawn_blocking` task that panicked resumes the panic with
`std::panic::resume_unwind` rather than raising an error.

## GC Slots and Lifetime Transmuting

When wrapping types that borrow from GC-managed values (e.g. a regex iterator
that borrows both a `Regex` and a haystack `str`), you cannot store the
borrowed references directly: the referents must stay alive, and their
contents must not move. The solution:

1. **Pin borrowed text with `PinStr`/`PinBin`** — `Str::pin` and `Bin::pin`
   return an owned pin that keeps the object alive and its contents in place
   until dropped. Store it in the object, declared _after_ the borrowing field
   so it drops after it.
2. **Declare `SLOTS` for other referents** — each slot is a GC-scanned `Value`
   that keeps a referent alive, such as the `Regex` whose compiled program the
   iterator borrows.
3. **Transmute borrowed lifetimes to `'static`** — strip the borrow's lifetime
   so it can be stored in the struct.

```rust
pub(crate) struct Find<'v> {
    // SAFETY: transmuted to 'static; the regex is kept alive by slot 0 and the
    // haystack by `_haystack`, which drops after `iter`
    iter: regex::CaptureMatches<'static, 'static>,
    _haystack: PinStr<'v>,
}

impl<'v> Object<'v> for Find<'v> {
    const SLOTS: usize = 1; // slot 0 = regex
    type Annex = FindAnnex<'v>;
    // ...
}
```

Population pattern (inside a method that creates the object):

```rust
// 1. Pin the haystack and create the borrowed iterator
let haystack = haystack_value.as_str(strand.vm()).unwrap().pin();
let iter = annex.regex.find_iter(&haystack);

// 2. Transmute to 'static (UNSAFE: the pin and slot must outlive it)
let iter = unsafe {
    mem::transmute::<regex::CaptureMatches<'_, '_>,
                     regex::CaptureMatches<'static, 'static>>(iter)
};

// 3. Create the object
global.types.find.create_with_annex(
    strand, Find { iter, _haystack: haystack }, FindAnnex { global }, &mut out,
);

// 4. Root the other referents in slots (keeps them alive for GC)
let mut borrow = global.types.find.downcast(&out).unwrap().borrow_mut_unwrap();
Output::set(strand, Mut::slot_mut::<0>(&mut borrow), this); // regex
```

Slots are accessed via `Ref::slot::<N>` (read) and `Mut::slot_mut::<N>` (write).
The const generic `N` is bounds-checked at compile time against `SLOTS`.

For text needed only within a call, use `with_str!`/`with_slice!` to borrow it
while keeping the strand, or `Str::as_str` with a `Strand::access` token;
neither needs a stored pin.

**Key invariant**: the transmuted references are only valid as long as their
referents are alive and pinned. Never clear or overwrite a slot that backs a
transmuted reference, and never drop a pin before its borrower.
