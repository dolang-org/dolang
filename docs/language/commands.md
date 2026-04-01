# Commands

At statement level, Do uses shell-like syntax where most tokens are literal
strings. This is the default parsing mode at the top level and within indented
blocks.

## Basic Commands

A command is a function call where the function name is followed by its
arguments, separated by whitespace:

```
echo hello world
# prints: hello world
```

The first position (the function to call) is parsed as a [compact
expression](expressions.md#compact-expressions) (covered in the next section),
so `echo` in the above example is looked up as a variable. The remaining tokens
are literal strings.

## Literal Strings

Most punctuation and characters that aren't reserved by the language are
treated as literal strings within command arguments:

```
echo https://example.com/path?query=1&other=2
# prints: https://example.com/path?query=1&other=2

echo 1+1
# prints: 1+1 (not 2!)

echo foo.bar.baz
# prints: foo.bar.baz
```

## Substitution

Use `$` to insert variable values (or more complex [compact
expressions](expressions.md#compact-expressions)).

```
let name = Alice
echo hello $name
# prints: hello Alice
```

The substitutions are *never* split into multiple arguments implicitly.

## Implicit Concatenation

Multiple `$` expressions not separated by whitespace are concatenated as
strings.

```
let hello = "hello "
let world = "world"
echo $hello$world
# prints: hello world
```

## Expression Arguments

Certain argument forms are always treated as expressions.

### Parentheses

```
echo (1 + 1)
# prints: 2
```

### Data Structure Literals

```
# Passes an array literal
func [1, 2]
# Passes a dictionary literal
func {"foo": "bar"}
```

### Quoted Strings

```
# Passes a quoted string
func "Hello, world!"
```

### Constants

```
# Passes an integer, bool, nil, and a symbol, respectively
func 1 false nil :symbol:
```

## Key Arguments

Key arguments use `key: value` syntax:

```
range end: 10
range start: 1 end: 10 step: 2
```

The value is treated as usual: interpreted as a literal string unless subject
to one of the above exceptions.

The `:key` shorthand passes a variable with the same name as a key argument:

```
let start = 1
let end = 10
range :start :end
# equivalent to: range start: $start end: $end
```

## Bare Names and Zero-Argument Calls

A bare name without arguments is always evaluated as a
[compact expression](expressions.md#compact-expressions), not a call. This
applies everywhere: at statement level, in `let`/assignment right-hand sides,
`if`/ `while` conditions, `for` iteratees, and `bind` scrutinees.

To call a function with no arguments, use `()`:

```
# Calls func with one argument -- this is a command
func arg1

# Evaluates the variable foo -- not a call
foo

# Calls foo with no arguments
foo()
```
