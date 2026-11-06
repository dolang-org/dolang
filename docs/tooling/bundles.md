# Script Bundles

A script bundle packages a program and its modules as one file: a zip archive,
optionally preceded by a `#!` line, which `dolang` runs as it runs a script.

## Format

The archive holds:

- `main.dol` -- the program
- `lib/` -- modules; `foo.bar` is `lib/foo/bar.dol` or `lib/foo/bar/mod.dol`
- `.dolc` files -- precompiled bytecode, used in place of a `.dol` source of
  the same name

Other members are ignored. Imports resolve inside the bundle first, then through
the [module search paths](../shell/modules.md#module-search-paths). Each module
in the bundle runs once. Bytecode for `main.dol` is compiled as a script, and
bytecode for a module in module mode under the module's name, as
`dolang -m compile bytecode --module` does. Bytecode that fails verification is
recompiled from its source when the bundle has one. Sources in a bundle compile
with no prelude, whatever the prelude options and `--strict` say, so a bundle
runs the same however it is invoked.

## Building

`dolang -m compile bundle` builds a bundle from a main module and the modules
named with `--module NAME=PATH`, writing each source beside its bytecode:

```bash
dolang -m compile bundle src/main.dol \
  --module util=src/util.dol \
  --module util.net=src/util/net.dol \
  --shebang '/usr/bin/env dolang' \
  -o app
```

With `--shebang`, the bundle starts with that `#!` line and is made executable
where the platform allows. The builder only compiles; run
`dolang -m compile check` first to check types. [`doze`](./doze.md#bundling)
checks and bundles a project laid out by convention instead.
