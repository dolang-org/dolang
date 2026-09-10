# Basic Types

Do is dynamically typed. Values carry their type at runtime, and variables can
hold any type.

`Int` and `Float` share the abstract [`Num`](std.Num) supertype and its common
rounding, sign, extrema, and clamping methods. The [`math`](math) module
provides transcendental functions and integer number theory.

## Integers (`Int`)

128-bit signed integers.

```
let x = 42
let y = -17
echo (x + y)  # 25
```

Integers support arithmetic (`+`, `-`, `*`, `/`, `//`, `%`), bitwise
(`&`, `|`, `^`, `~`, `<<`, `>>`), and comparison operators.

`/` performs floating point division and results in a float. The `//` operator
performs Euclidean division and `%` computes the Euclidean remainder, such that
`x == (x // y) * y + (x % y)`. This means both always yield `Int`s as
results.

## Floats (`Float`)

64-bit floating point.

```
let pi = 3.14
let sci = 5.5e-2
echo $pi       # 3.14
echo $sci      # 0.055
```

`//` and `%` likewise perform Euclidean division and remainder operations for
floats, meaning that `//` always returns an `Int` while `%` returns a `Float`.

## Strings (`Str`)

Immutable UTF-8 strings, written as bare literals at statement level, as
quoted strings, or as multi-line here strings:

```
echo "Quoted strings" or bare literals
```

## Binary Strings (`Bin`)

Immutable byte sequences that may contain arbitrary (non-UTF-8) data, written
with a `b"..."` prefix:

```
let data = b"\x01\x02\x03"
```

See [Strings](strings.md) for every literal form of both types, along with
escaping and interpolation. `Str` and `Bin` also support a wide variety of
methods ([`Str`](std.Str), [`Bin`](std.Bin)).

## Booleans (`Bool`)

`true` and `false`.

## Nil (`nil`)

A generic "absent" marker and the result of functions, expressions, and
statements with nothing interesting to return.

## Symbols (`Sym`)

Globally canonical identifiers that are more efficient to hash and compare than
`Str`. Used for literal dictionary keys, key arguments in variadic argument
packs, object fields, ad-hoc enumerated constants, etc.

```
let value = some_dict[:key:]
let mode = :VERBOSE:
```

Symbols can also be created from strings with `sym`:

```
let s = sym "my_symbol"
```

## Data Structures

See [Data Structures](./data-structures.md) for arrays, dictionaries, records,
sets, and tuples.

## Type Inspection

Every value has an associated **type object** that represents its type. The
built-in types (`Int`, `Float`, `Str`, `Bool`, `Sym`, `Array`, `Dict`, etc.)
each have a corresponding type object available in the standard library.

`Type` is the type of types. Use the lowercase `type` function to query or
test types:

```
# Get the type of a value (returns the type object)
assert_eq (type 42) $Int
assert_eq (type "hello") $Str
assert_eq (type [1, 2]) $Array
assert_eq (type nil) $Nil

# Test if a value is an instance of a type
assert (type 42 Int)
assert (type "hello" Str)
assert (type nil Nil)
```

User-defined classes work the same way:

```
class Foo
  field x = 0

let f = Foo()
assert_eq (type f) $Foo
assert (type f Foo)
```

See [Classes](./classes.md) for defining your own types.

## Type Conversions

The built-in types can be called as functions to convert values:

```
assert_eq (str 42) "42"
assert_eq (int "42") 42
assert_eq (float 42) 42.0
assert_eq (bool 0) false
assert_eq (bool 1) true
```
