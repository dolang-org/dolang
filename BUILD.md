# Building Do

Do uses [`dodo.dol`](./dodo.dol) as its build script. Once Do is installed,
run its rules through `dodo`:

```
dodo build
dodo test
```

Use `dodo --list` to list the rules and `dodo <rule> --help` to inspect a
rule's arguments.

## Prerequisites

Building from source requires:

- Rust 1.93 or later with Cargo
- A C/C++ build toolchain and libclang
- Git

On Linux, `dodo install` builds a static musl `dolang-vfs`. Install the musl
compiler tools, `pkg-config`, and the zstd development package, then add the
Rust musl target for the host architecture. For example, on x86-64:

```
rustup target add x86_64-unknown-linux-musl
```

Documentation and specialized rules have additional prerequisites described
under [Optional tooling](#optional-tooling).

## Bootstrap from a Release

The [GitHub releases page](https://github.com/dolang-org/dolang/releases)
provides bootstrap archives named for their target platform:

- `dolang-x86_64-linux.tar.gz`
- `dolang-aarch64-linux.tar.gz`
- `dolang-x86_64-windows.tar.gz`
- `dolang-aarch64-windows.tar.gz`
- `dolang-aarch64-macos.tar.gz`
- `dolang-x86_64-freebsd.tar.gz`

Download the archive for the host, extract its contents into `<PREFIX>/bin`,
and add that directory to `PATH`. The archive contains `dolang`, `dolang-lsp`,
and `dolang-vfs`. Unix archives also contain the `dodo` and `dolang-test`
symlinks.

For example:

```
mkdir -p <PREFIX>/bin
tar -xzf dolang-x86_64-linux.tar.gz -C <PREFIX>/bin
export PATH="<PREFIX>/bin:$PATH"
dodo --list
```

Replace `<PREFIX>` with an actual path, such as `$HOME/.local`.

Windows archives do not contain symlinks. Run the bundled build entrypoint
directly:

```
dolang.exe -m dodo --list
dolang.exe -m dodo build
```

Alternatively, create `dodo.exe` as a symlink to `dolang.exe` in the same
directory. Creating symlinks on Windows may require Developer Mode or an
elevated shell.

## Bootstrap from Source

Clone the repository and use Cargo to run the checked-out interpreter against
the checked-out build script. Only the extensions needed by `dodo.dol` are
enabled for this initial build:

```
git clone https://github.com/dolang-org/dolang.git
cd dolang
cargo run --bin dolang --no-default-features \
  --features json,digest,base64,rand,regex -- \
  -m dodo install --prefix <PREFIX>
```

The reduced feature set only minimizes the temporary interpreter Cargo builds
to start `dodo`. The `install` rule then builds the complete distribution
profile and installs the main programs into `<PREFIX>/bin`. If that directory
is not writable, the rule requests privilege elevation.

On Unix, the installation also creates `<PREFIX>/bin/dodo` and
`<PREFIX>/bin/dolang-test` as symlinks to `dolang`. Add the directory to
`PATH`, then use `dodo` for subsequent work:

```
export PATH="<PREFIX>/bin:$PATH"
dodo build
```

On Windows, `install` does not create either symlink. Continue to use
`dolang.exe -m dodo`, or create a `dodo.exe` symlink yourself.

## Common Workflow

Format and validate a change with:

```
dodo fmt
dodo lint
dodo test
dodo fmt-docs
dodo lint-docs
```

`build` is the default rule, so running `dodo` without a rule is equivalent to
`dodo build`.

Most build and test rules accept the shared `--target` option. Supported names
are `x86_64-linux`, `aarch64-linux`, `x86_64-windows`, `aarch64-windows`,
`x86_64-freebsd`, and `aarch64-macos`. The host target is selected by default:

```
dodo build --target aarch64-linux
```

Rules that forward arbitrary arguments use `--` to separate dodo options from
the underlying command's options:

```
dodo build -- --workspace
dodo cargo-test -- --package dolang-runtime
dodo shell-test -- --tags parser test
```

## Rules

### Build and installation

| Rule      | Purpose                                                                            |
| --------- | ---------------------------------------------------------------------------------- |
| `build`   | Builds the workspace with Cargo. Additional arguments are passed to `cargo build`. |
| `run`     | Builds and runs `dolang`. Additional arguments are passed to the interpreter.      |
| `install` | Builds distribution binaries and installs them under `--prefix`.                   |
| `clean`   | Removes Cargo output and generated site and graph output.                          |

### Formatting and tests

| Rule                      | Purpose                                                                                             |
| ------------------------- | --------------------------------------------------------------------------------------------------- |
| `fmt`                     | Formats Rust with `cargo fmt`.                                                                      |
| `lint`                    | Runs Clippy for all targets, then checks Rust formatting.                                           |
| `cargo-test`              | Runs Cargo tests. Additional arguments are passed to `cargo test`.                                  |
| `shell-test`              | Runs the Do integration tests. By default, slow and release-tagged tests are excluded.              |
| `test`                    | Runs both Cargo and Do integration tests. Use `--slow` or `--release` to include those test groups. |
| `cargo-bench`             | Runs Cargo benchmarks with the optimized distribution profile.                                      |
| `test-asan`               | Runs tests under AddressSanitizer with nightly Rust.                                                |
| `test-miri`               | Runs selected crates under Miri with nightly Rust.                                                  |
| `gen-bytecode-fuzz-seeds` | Generates the bytecode fuzzer's seed corpus.                                                        |
| `fuzz-bytecode`           | Regenerates the seed corpus and runs the bytecode fuzzer.                                           |

Passing explicit test-runner arguments to `shell-test` replaces its default
tag selection. `test --release` also increases the integration-test timeout.

### Coverage

| Rule        | Purpose                                                           |
| ----------- | ----------------------------------------------------------------- |
| `cov`       | Collects workspace and integration-test coverage.                 |
| `cov-dump`  | Writes summary JSON, full JSON, and text coverage reports.        |
| `serve-cov` | Builds an HTML coverage report and serves it on `localhost:8080`. |

Run `dodo cov` before `cov-dump` or `serve-cov`.

### Documentation

| Rule            | Purpose                                                              |
| --------------- | -------------------------------------------------------------------- |
| `fmt-docs`      | Formats Markdown with rumdl.                                         |
| `lint-docs`     | Checks Markdown with rumdl.                                          |
| `mkdocs`        | Builds the MkDocs site in strict mode.                               |
| `serve-mkdocs`  | Builds and serves the MkDocs site locally.                           |
| `rustdoc`       | Builds Rust API documentation for the public Rust crates.            |
| `serve-rustdoc` | Builds and serves Rust API documentation on `localhost:8080`.        |
| `pages`         | Builds the complete GitHub Pages tree, including MkDocs and rustdoc. |

### Maintenance and release engineering

| Rule                     | Purpose                                                                                  |
| ------------------------ | ---------------------------------------------------------------------------------------- |
| `gen-system-error-codes` | Regenerates system error lookup tables. Arguments are passed to the generator.           |
| `verify-macos-interpose` | Checks that the macOS `posix_spawn` interpose section survived linking.                  |
| `publish`                | Publishes workspace crates in dependency order. Arguments are passed to `cargo publish`. |

These rules update generated source, verify release artifacts, or publish
packages. They are not part of the normal edit-test cycle.

## Optional Tooling

- Documentation: Python packages from `docs/requirements.txt` and `rumdl`
- Rust API documentation: a nightly Rust toolchain
- AddressSanitizer: nightly Rust and the components needed by `-Z build-std`
- Miri: the nightly `miri` component
- Fuzzing: nightly Rust and `cargo-fuzz`
- Coverage: `cargo-llvm-cov`
- System error generation: Python 3

Install only the tools needed for the rules you intend to run.
