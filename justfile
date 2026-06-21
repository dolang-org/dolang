set positional-arguments

asan_target := "x86_64-unknown-linux-gnu"
shell_vfs := "target/debug/dolang-shell-vfs"
llvm_cov_shell_vfs := "target/llvm-cov-target/debug/dolang-shell-vfs"
shell_test_args := "-m test -- test/*.dol"
mkdocs_bins := "--bin dolang-highlight --bin dolang-doc"
mkdocs_env := "DOLANG_HIGHLIGHT=target/release/dolang-highlight DOLANG_DOC=target/release/dolang-doc"
cov_ignore := "(dolang-private-util/src/hashbrown|dolang-private-doc|dolang-private-highlight|dolang-private-build)"

# Run tests with AddressSanitizer (slow to build)
test-asan *args:
    env \
        RUSTFLAGS="-Z sanitizer=address" \
        cargo +nightly build --profile asan --target {{asan_target}} \
        --bin dolang-shell-vfs -Z build-std "$@"
    env \
        RUSTFLAGS="-Z sanitizer=address" \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/target/{{asan_target}}/asan/dolang-shell-vfs" \
        cargo +nightly test --profile asan --target {{asan_target}} \
        -Z build-std "$@"
    env \
        RUSTFLAGS="-Z sanitizer=address" \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/target/{{asan_target}}/asan/dolang-shell-vfs" \
        cargo +nightly run --profile asan --bin dolang --target {{asan_target}} \
        -Z build-std "$@" -- {{shell_test_args}}

# Run tests with miri (VERY SLOW)
test-miri *args:
    env \
        MIRIFLAGS="-Zmiri-disable-isolation" \
        cargo +nightly miri test \
        -p dolang-bytecode \
        -p dolang-compile \
        -p dolang-runtime \
        -p dolang-private-util \
        -p dolang \
        -p dolang-private-regression \
        "$@"

cargo-test *args:
    cargo build --bin dolang-shell-vfs "$@"
    env \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/target/debug/dolang-shell-vfs" \
        cargo test "$@"

gen-bytecode-fuzz-seeds:
    cargo run --manifest-path fuzz/Cargo.toml --bin gen-bytecode-seeds

fuzz-bytecode *args:
    just gen-bytecode-fuzz-seeds
    cargo +nightly fuzz run bytecode_deserialize \
        {{justfile_directory()}}/target/fuzz-corpus/bytecode_deserialize \
        "$@"

fuzz-bytecode-smoke:
    just fuzz-bytecode -- -runs=1000 -max_len=1048576

test *args:
    just cargo-test "$@"
    cargo build --bin dolang-shell-vfs "$@"
    env \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/{{shell_vfs}}" \
        cargo run --bin dolang "$@" -- {{shell_test_args}}

cov *args:
    cargo llvm-cov clean
    cargo llvm-cov --no-report run --ignore-run-fail --bin dolang-shell-vfs --all-features "$@"
    env \
        DO_EXPORT_DOT=`pwd`/dot \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/{{llvm_cov_shell_vfs}}" \
        cargo llvm-cov --no-report test --all-features "$@"
    env \
        DOLANG_SHELL_VFS="{{justfile_directory()}}/{{llvm_cov_shell_vfs}}" \
        cargo llvm-cov run --no-report --bin dolang --all-features "$@" -- \
        {{shell_test_args}}

cov-dump:
    cargo llvm-cov report --json --summary-only --output-path cov.json \
        --ignore-filename-regex '{{cov_ignore}}'
    cargo llvm-cov report --json --output-path cov.full.json \
        --ignore-filename-regex '{{cov_ignore}}'
    cargo llvm-cov report --text --output-path cov.full.txt \
        --ignore-filename-regex '{{cov_ignore}}'

serve-cov:
    cargo llvm-cov report --html \
        --ignore-filename-regex '{{cov_ignore}}'
    python3 -m http.server 8080 -d target/llvm-cov/html/ -b localhost

rustdoc:
    cargo doc -p dolang-runtime -p dolang-compile -p dolang-ext-shell --no-deps

# Build docs and serve them locally with Python's HTTP server
serve-rustdoc:
    cargo doc --package dolang --package dolang-ext-shell --no-deps
    python3 -m http.server 8080 -d target/doc -b localhost

# Serve MkDocs documentation site locally
serve-mkdocs:
    cargo build --release {{mkdocs_bins}}
    env {{mkdocs_env}} mkdocs serve

# Build MkDocs documentation site
mkdocs:
    cargo build --release {{mkdocs_bins}}
    env {{mkdocs_env}} mkdocs build

fmt:
    cargo fmt
    rumdl fmt

clean:
    cargo clean
    rm -rf site dot

# Install binaries
install *args:
    @if [ "$(uname)" = "Linux" ]; then \
        env ZSTD_SYS_USE_PKG_CONFIG=1 \
            cargo install --profile dist --path dolang-shell --bin dolang "$@"; \
    else \
        cargo install --profile dist --path dolang-shell --bin dolang "$@"; \
    fi
    cargo install --profile dist --path dolang-lsp "$@"
    # Install dolang-shell-vfs with musl target on Linux
    @if [ "$(uname)" = "Linux" ]; then \
        cargo install --profile dist --target x86_64-unknown-linux-musl --path dolang-shell-vfs "$@"; \
    fi

# Publish workspace crates in dependency order.
publish *args:
    cargo publish -p dolang-private-build "$@"
    cargo publish -p dolang-private-util "$@"
    cargo publish -p dolang-private-ipc "$@"
    cargo publish -p dolang-bytecode "$@"
    cargo publish -p dolang-compile "$@"
    cargo publish -p dolang-runtime "$@"
    cargo publish -p dolang "$@"
    cargo publish -p dolang-private-test "$@"
    cargo publish -p dolang-shell-vfs "$@"
    cargo publish -p dolang-ext-compile "$@"
    cargo publish -p dolang-ext-json "$@"
    cargo publish -p dolang-ext-url "$@"
    cargo publish -p dolang-ext-regex "$@"
    cargo publish -p dolang-ext-glob "$@"
    cargo publish -p dolang-ext-base64 "$@"
    cargo publish -p dolang-ext-digest "$@"
    cargo publish -p dolang-ext-rand "$@"
    cargo publish -p dolang-ext-xml "$@"
    cargo publish -p dolang-ext-yaml "$@"
    cargo publish -p dolang-ext-shell "$@"
    cargo publish -p dolang-ext-http "$@"
    cargo publish -p dolang-ext-zip "$@"
    cargo publish -p dolang-ext-sqlite "$@"
    cargo publish -p dolang-ext-progress "$@"
    cargo publish -p dolang-ext-diagnostic "$@"
    cargo publish -p dolang-ext-load "$@"
    cargo publish -p dolang-shell "$@"
    cargo publish -p dolang-lsp "$@"
