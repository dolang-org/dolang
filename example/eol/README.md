# eol

An example [Do project](../../docs/tooling/index.md): a command-line client for
the [endoflife.date](https://endoflife.date) API, which reports when releases of
products such as Python, Ubuntu and PostgreSQL stop being supported.

```
doze check --strict
doze test --tags '!network'
doze bundle
dolang build/eol.dolz check python 3.12
```

`check` exits with status 1 when a version is end-of-life or matches no
release, so it can gate a CI job:

```
dolang build/eol.dolz check --from versions.toml
```

where `versions.toml` pins product versions:

```toml
python = "3.12"
postgresql = "16.4"
```

## Layout

- `main.dol`: the program.
- `lib/eol/mod.dol`: module `eol`, a client for the API.
- `lib/eol/status.dol`: module `eol.status`, which classifies releases by date.
- `test/`: tests, which run against a mock of the API. `test/live.dol` calls the
  real API; it's tagged `network` and skipped unless `EOL_LIVE` is set.
