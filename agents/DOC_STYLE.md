# Documentation Style

Keep prose in `docs/` and in doc comments direct and technical. The recurring
problems these address were AI-generated verbosity — if a sentence reads like
filler, cut it.

## API Reference

The API reference under `docs/api/` is generated from doc comments in Do
sources. Never write or edit pages there; the directory is not checked in.
`dodo mkdocs` extracts each module with `dolang -m compile extract --doc`,
writes a page for the module and one for each documented public class, and
renders them with the mkdocstrings handler in `docs/mkdocstrings_handlers/do/`.
LSP hover text for imported and prelude names comes from the same extraction.

Documented modules live in:

- `dolang-shell-modules/lib/` — modules written in Do, documented where they are
  implemented.
- `dolang/stub/` and `dolang-ext-*/stub/` — stubs for native modules. A
  stub declares a module's public API, with `...` as each body, to carry
  its documentation and type annotations. Keep it in step with the native
  module.

A file `foo/bar.dol` documents module `foo.bar`. Only `pub` declarations are
extracted, and the pages leave out undocumented ones.

### Doc Comments

A block of comment lines directly above a declaration documents it; a blank line
ends a block. A block on the first line of a file (after any `#!` line)
documents the module, so a blank line must separate it from the first
declaration's block.

The handler lays out the page: signature headings, parameter tables, and the
tables of a module's types and functions. A doc comment holds only prose, in
this order:

1. A brief first paragraph. A class's first paragraph is its summary in the
   module's table of types.
2. Further paragraphs, if needed.
3. Sections as `##` headings, omitting any that don't apply: `## Returns` (only
   when it says more than the return type), `## Errors`, `## Example`.

A short code fence may follow the opening paragraphs without a heading. Once a
section begins, a later code fence goes under its own `## Example` rather than
inheriting the preceding section. Don't use bold text as a substitute for a
section heading.

When more than one exception is worth documenting, use a table with
`Exception` and `Condition` columns so each exception's meaning is explicit.
Single exceptions may use concise prose. Do not document incidental exceptions
such as cancellation or interruption unless they are part of the API's
specific contract.

A parameter's doc comment goes on the parameter itself, so declare documented
parameters vertically. The first paragraph becomes the parameter's row in the
table; later paragraphs become a subsection of its own, which may use nested
headings. A declaration with a parameter table has its signature heading
abbreviated to its first few required positional parameters.

```
# Pulls an image.
pub def pull
  # Image name or ID.
  image @ Str
  # Registry to pull from; the configured default when omitted.
  :registry @ Str = nil
do -> Image
  ...
```

### Types

Annotations supply the Type column, the return type after a signature heading,
and a field's type, with names linked to their documentation:

- Annotate what a caller may pass. A keyword to omit rather than pass `nil` is
  still `:limit @ Int = nil`; a parameter that accepts `nil` is
  `@ (Int | nil)`.
- A native property is a method marked `#[getter]`, with its type as the return
  type: `pub def len self -> Int`.
- Name a type from another module with a type-only import item (`- @Iter`) when
  the module is otherwise unused.
- Leave out an annotation on a parameter that accepts anything.

### Declarations in Stubs

- Write a declaration without parameters as `def name()`; `def name` directly
  followed by its body is a syntax error.
- A constructor is the special method `(init)`. A method on the type object is
  marked `#[class]` and still declares `self`.
- Qualify a superclass from another module: `class Error: std.RuntimeError`. A
  class names one runtime supertype; name any other type it implements as a
  type-only supertype, as in `class Blake3: @State`.
- Give a module value its literal when it has one (`pub let PI @ Float =
  3.141592653589793`), and otherwise `...`, as in `pub let args @ Args = ...`.
  Fields whose values the native implementation supplies also use `...`, as in
  `pub field name @ Str = ...`. Protocol fields cannot have defaults.
  Evaluating a `...` value raises an error, as calling a stub function does.

### Links

Link to another documented name by its identifier, as in
``[`Str`](std.Str)`` or ``[`Regex.match`](regex.Regex.match)``. A doc comment's
relative links resolve against the page that renders it, not the source file.
Link to a heading on the same page through the page itself
(`](./index.md#anchor)`), never a bare `](#anchor)`.

## Voice and Brevity

- **Lead with what it does, not what it is.** One-line descriptions should
  state the function/type's purpose directly, not narrate it.
    - Good: `Computes the BLAKE3 digest.`
    - Bad: `Computes the BLAKE3 digest of a string or binary value and returns
      the raw digest bytes.` (the signature already says what it takes and
      returns)
- **Don't restate what the annotations show.** Prose shouldn't repeat a
  parameter's or return value's type.
- **Don't list interface methods on every concrete type.** If `Blake3`
  implements `State`, say so once — don't re-list `update`, `digest`, etc.
  on the `Blake3` page. Link to `State` instead.
- **Use Do-native terminology.** Say "callable" or "block", not "thunk". Say
  "supertype", not "nominal base type".

## Code Examples

- **Use plain fences.** The MkDocs setup handles highlighting without a language
  tag. A fence tagged `playground` renders the same, plus a link that opens the
  example in the browser playground; use it for examples that run there.
- **Don't include `import` lines** in examples unless the example is
  specifically about importing. API doc examples should assume the module's
  exports are in scope. In a `playground` fence, write the imports the example
  needs on lines starting with `#>`, which the page hides but the playground
  runs.
- **Use Do idioms in examples.** Prefer `$x.method()` and vertical layout
  over wrapping everything in `(...)`. Break long lines with here strings
  or vertical argument lists, not by cramming into one line.
- **Module-qualify only when the reader might be confused.** Within a module's
  own doc page, use bare names: `Blake3()` not `digest.Blake3()`.

## Factual Accuracy

- **Don't invent behavior.** If you haven't verified how a function handles
  edge cases, don't document edge-case behavior. Missing docs can be added
  later; wrong docs cause bugs.
- **Don't document removed or renamed features.** If the code doesn't have
  a "current directory" module search, don't document it. Check the
  implementation when uncertain.
- **Avoid "notes" that merely re-explain the obvious.** E.g. if `[]` is a
  full expression context, you don't need a parenthetical reminding the
  reader that keys must be quoted strings.
