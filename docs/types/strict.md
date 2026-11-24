# Strict Checking

A file opts into strict checking with a directive comment on one of its first 10
lines:

```
# dolang: strict
```

A strict file must annotate its named functions' parameters and its classes'
fields, as well as `pub let` values. A method's receiver is implicitly typed by
its class and need not be annotated.

Omitted annotations that remain have conservative types rather than unknown
ones:

- An omitted return type is `nil`
- An omitted `<` or `>` parameter is `Value` in a block/lambda's body. In a
  `def`, it is universally quantified.

A pattern that could throw an error when it doesn't match (in `let`, `bind`,
`for`, or a block's parameters) gets a warning in a strict unit. To handle a
mismatch, give a `let` or `bind` an
[`else`](../language/patterns.md#let-bind-with-else) or use a `match` statment.

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
```

Code the checker can't verify, such as a form it doesn't handle, gets a warning
in a strict file, with a note giving the reason.

To leave a type unchecked in a strict file, annotate it with
[`std.Unknown`](std.Unknown), the unknown type, rather than omitting the
annotation:

```
def (get) self name @ Str -> std.Unknown
  self.#values[name]
```

`# dolang: nostrict` turns strict checking off. Strictness belongs to the file,
so strict and non-strict modules can import each other.
