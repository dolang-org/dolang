# diagnostic

Renders compiler diagnostics and runtime errors.

## Functions

### `print_compile_diag diag :color?`

Prints a [`compile.Diagnostic`](./compile/diagnostic.md) to stderr.

#### Parameters

| Name    | Type | Description                                             |
| ------- | ---- | ------------------------------------------------------- |
| `diag`  |      | Compiler diagnostic to render                           |
| `color` | `?`  | Optional color mode: `:auto:`, `:never:`, or `:always:` |

#### Returns

`nil`

```
let result = compile.compile "bad.dol" "let =\n"
for diag = result.diagnostics
  diagnostic.print_compile_diag $diag
```

### `render_compile_diag diag :color?`

Renders a [`compile.Diagnostic`](./compile/diagnostic.md) to a string.

#### Parameters

| Name    | Type | Description                                             |
| ------- | ---- | ------------------------------------------------------- |
| `diag`  |      | Compiler diagnostic to render                           |
| `color` | `?`  | Optional color mode: `:auto:`, `:never:`, or `:always:` |

#### Returns

[`str`](./std/str.md)

```
let text = diagnostic.render_compile_diag $diag color: :never:
```

### `print_error err :backtrace? :color?`

Prints an error message and backtrace to stderr.

#### Parameters

| Name        | Type | Description                                             |
| ----------- | ---- | ------------------------------------------------------- |
| `err`       |      | Value to render as the error message                    |
| `backtrace` | `?`  | Optional [`strand.Backtrace`](./strand/index.md)        |
| `color`     | `?`  | Optional color mode: `:auto:`, `:never:`, or `:always:` |

#### Returns

`nil`

#### Errors

| Exception      | Condition                                                       |
| -------------- | --------------------------------------------------------------- |
| `TypeError`    | `backtrace` is provided and is not `strand.Backtrace`           |
| `RuntimeError` | `backtrace` is omitted and there is no active handled exception |

```
try
  throw "boom"
catch e
  diagnostic.print_error $e
```

### `render_error err :backtrace? :color?`

Renders an error message and backtrace to a string.

#### Parameters

| Name        | Type | Description                                             |
| ----------- | ---- | ------------------------------------------------------- |
| `err`       |      | Value to render as the error message                    |
| `backtrace` | `?`  | Optional [`strand.Backtrace`](./strand/index.md)        |
| `color`     | `?`  | Optional color mode: `:auto:`, `:never:`, or `:always:` |

#### Returns

[`str`](./std/str.md)

```
let text = try
  throw "boom"
catch e
  diagnostic.render_error $e color: :never:
```
