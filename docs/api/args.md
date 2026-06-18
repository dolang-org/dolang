# args

Declarative command-line argument parsing.

## Spec declarations

Both `args.parse` and `args.with` accept `help:` and `usage:` as keyword
arguments, plus any number of positional spec items using `-` list syntax.
Each spec item is a dict whose first key is both the item type and its name:

```
args.parse
  help: Overall description
  - flag: verbose
  - opt: format
    short: f
    default: gz
  - arg: file
  - cmd: deploy
    - arg: service
    do |p|
      echo "Deploying $(p.deploy.service)"
```

### `- flag: <name>`

A boolean flag. Present on the command line sets it to `true`; absent defaults
to `false`. Additional settings:

- `long:` — the `--long-flag` name (default: same as `name`); set to `nil`
  for short-only flags with no long form
- `short:` — single-character shorthand (e.g. `v` → `-v`)
- `default:` — override the default value (rarely needed; must be `true` or
  `false`)
- `env:` — environment variable name to use as fallback when the flag is
  not provided on the command line
- `help:` — description shown in `--help` output

`name` becomes the result record key (hyphens converted to underscores:
`dry-run` → `p.dry_run`). `long` controls the CLI flag independently.

### `- opt: <name>`

A value-consuming named option. Additional settings:

- `long:` — the `--long-option` name (default: same as `name`); set to `nil`
  for short-only options with no long form
- `type:` — 1-argument coercion callable applied to the raw string value
- `short:` — single-character shorthand (e.g. `f` → `-f`)
- `default:` — default value; omitting `default:` makes the option required
- `env:` — environment variable name to use as fallback when the option is
  not provided on the command line
- `collect: true` — accept the option multiple times; result is an array
- `meta:` — metavar shown in help output (default: uppercased name)
- `values:` — array of allowed values; others are rejected
- `help:` — description shown in `--help` output

`name` becomes the result record key (hyphens converted to underscores:
`dry-run` → `p.dry_run`). `long` controls the option syntax independently
if you need them to differ.

### `- arg: <name>`

A positional argument. Additional settings:

- `type:` — 1-argument coercion callable applied to the raw string value
- `default:` — default value; omitting `default:` makes the argument required
- `collect: true` — absorb all remaining positional arguments into an array;
  only the last `arg:` may have `collect: true`
- `values:` — array of allowed values
- `help:` — description shown in `--help` output

### `- cmd: <name>`

A subcommand. Additional items:

- `help:` — description shown in `--help` output
- Nested `- flag:`, `- opt:`, `- arg:`, and `- cmd:` items for the subcommand's
  own arguments
- `handler` (positional) — handler called by `args.with` with the parsed record

When a subcommand is matched, `p.cmd` is set to the normalized symbol of the
matched subcommand name (e.g. `:deploy:` or `:build_docs:`). The selected
subcommand's fields are stored in a nested record at `p[p.cmd]`, alongside any
global options on the top-level result. The effective handler is available at
`p.handler`, so callers using `args.parse` may invoke it themselves. When help
is requested, `p.help` is set to the rendered help text string and parsing
returns early.

### `args: <array>`

The argument list to parse. Defaults to `shell.args`.

### `program: <str>`

The program name used in `--help` output and error messages. Derived from
`shell.program` by default — the stem of the script filename for scripts, or the
module name for modules.

### `help: <str>`

Description paragraph shown below the usage line in `--help` output.

### `usage: <str>`

Override the auto-generated `Usage:` line. The program name is prepended
automatically.

---

::: args
