# dolang-private-util Architecture

The `dolang-private-util` crate provides low-level utilities used throughout the
Do implementation.

## Interning (`intern.rs`)

`BinTable` interns byte strings through `&self`. It copies each one whole into
the last of a list of doubling heap segments, starting a new segment when it
doesn't fit, and indexes them with a `MonoHashMap` from the bytes to a logical
start offset. Offsets are assigned densely in insertion order, so `flatten`
concatenates the segments' filled prefixes into the contiguous table that
bytecode files store. `BinId`/`StrId` are entry indices; `range` gives their
logical offsets. `Table` interns sized types through `&self`,
built on `MonoHashMap`; unindexed "fresh" entries share its storage but are
never found by lookup. `Id` stores the index plus one as a `NonZeroU32`.

## Monotonic Collections (`mono.rs`)

These collections grow through `&self` and never move their elements, so
references to elements remain valid as they grow.

`MonoVec<T>` is an append-only vector that grows in exponentially-sized chunks,
with O(1) push.

`MonoHashMap<K, V>` stores entries in insertion order in a `MonoVec` and indexes
them with a raw `hashbrown` table of entry indices behind a `RefCell`. Growth
rehashes from stored hashes and frees the old index at once. Inserting an
existing key fails. Accessing the map from a key's `Eq` during insertion panics.
`MonoHashSet<T>` wraps `MonoHashMap<T, ()>`.

## Intrusive Linked Lists (`ring.rs`)

`Ring` implements a doubly-linked circular list with zero-allocation operations
using compile-time offset calculation.

## Future Pinning (`pin.rs`)

`Arena` provides a segmented arena for pinning futures to amortize heap
allocation costs. Since deallocation can only occur in LIFO order, the API is
inherently unsafe and requires a wrapper which enforces LIFO order (e.g. via
ownership of a non-`Copy` token) to prevent attempts to pin concurrent
outstanding futures.

## Verification Wrapper (`verified.rs`)

`Verified<T>` is a transparent wrapper indicating a value has been validated
to represent input sanitization state in the type system.

## Hashing

Vendored copy of most of `hashbrown` (the standard Rust hash table) for use in
implementing the Do `dict` type, as only the low-level raw API is general enough
for that purpose.
