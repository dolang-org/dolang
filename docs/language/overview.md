# Overview

Do is a dynamically-typed scripting language with indentation-based syntax.
Block structure is determined by indentation (typically 2 spaces).

## Syntactic Levels

Do has three broad syntactic levels that determine how code is parsed:

### Statement Level

At the top level and within indented blocks, syntax is **shell-like**. Most
tokens are treated as literal strings, whitespace separates arguments, and `$`
introduces variable references and expressions:

```
echo hello world
echo 1+1 https://example.com
# prints: hello world
# prints: 1+1 https://example.com
```

### Full Expression Level

Within parentheses `()`, brackets `[]`, and braces `{}`, syntax switches to
**C-like expressions** where whitespace is insignificant and operators work as
expected:

```
echo (1 + 1)
let arr = [1, 2, 3]
let dict = {name: "Alice", age: 30}
```

### Compact Expression Level

The `$` token introduces a **compact expression** that allows variable access,
field access, indexing, and C-style calls without full parentheses:

```
echo $name
echo $person.age
echo $arr[0]
echo $factorial(5)
```

Several positions are compact expressions by default (no `$` needed):

- The function being called: `echo` in `echo hello`
- Conditions in `if` and `while`
- The iteratee in `for`
- The scrutinee in `bind`
- The right-hand side of `let` and assignment
- The value in `return`
