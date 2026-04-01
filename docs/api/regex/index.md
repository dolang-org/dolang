# regex

The `regex` module provides compiled regular expression matching using
[RE2-compatible syntax](https://docs.rs/regex/latest/regex/#syntax).

## Usage

```
let pattern = Regex r"\d+"
let caps = pattern.match "abc 42 def"
echo caps  # => 42
```

## Types

- [Regex](./regex.md) - A compiled regular expression

## Syntax

Patterns use RE2 syntax, the same as the Rust
[`regex`](https://docs.rs/regex/latest/regex/#syntax) crate. This supports
most common regular expression features including:

- Character classes: `\d`, `\w`, `\s`, `[a-z]`, `[^abc]`
- Repetition: `*`, `+`, `?`, `{n}`, `{n,m}`
- Anchors: `^`, `$`, `\b`
- Groups: `(abc)`, `(?:abc)` (non-capturing), `(?<name>abc)` (named)
- Alternation: `a|b`

Backreferences and lookahead/lookbehind assertions are not supported.
