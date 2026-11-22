# Strict Checking

A file opts into strict checking with a directive comment on one of its first 10
lines:

```
# dolang: strict
```

A strict file must annotate its named functions' parameters and its classes'
fields. A method's receiver is still typed by its class, and a `do` block still
takes its types from where it is passed. Omissions that remain have conservative
types rather than unknown ones:

- An omitted return type is `nil`

- An omitted `<` or `>` is `Value` in the function's body, so it neither reads
    its input nor writes its output. A function that reads its input, or calls
    one that does, annotates `<`, and one that writes annotates `>`. So does a
    `do` block that isn't passed straight to a call and has no expected type
    from an annotation or cast:

    ```
    let report = do |>Sink[Str]|
      put done
    ```

A pattern that raises when it doesn't match, in `let`, `bind`, `for`, or a
block's parameters, must match every value its scrutinee's type admits. A
strict file gets a warning when the value may lack an item the pattern requires,
or have one it doesn't take. Type tests and constants in the pattern are
exempt. To handle a mismatch, give the `let` or `bind` an
[`else`](../language/patterns.md#let-bind-with-else):

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
```

Code the checker can't verify, such as a form it doesn't handle, gets a warning
in a strict file, with a note giving the reason.

To leave one declaration unchecked in a strict file, annotate it with
[`std.Unknown`](std.Unknown), the unknown type, rather than omitting the
annotation:

```
def (get) self name @ Str -> std.Unknown
  self.#values[name]
```

`# dolang: nostrict` turns strict checking off. Strictness belongs to the file,
so strict and non-strict modules can import each other. The `--fail` option is
unrelated, and only chooses which diagnostics fail a run or check.
