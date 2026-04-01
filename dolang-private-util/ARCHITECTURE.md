# dolang-private-util Architecture

The `dolang-private-util` crate provides low-level utilities used throughout the
Do implementation.

## Interning (`intern.rs`)

`StrTable` provides string deduplication by storing everything in a single
`String` with a `HashMap` index. `Table` provides for generic interning of
sized types.

## Arena Vector (`arena.rs`)

`ArenaVec<T>` is an append-only vector that grows in exponentially-sized chunks.
Elements are never moved after insertion, enabling stable references with O(1)
push operations via `&self`.

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
