# Esker task runner. `just check` is the gate: it is exactly what CI runs, so a green
# check locally means a green CI (CLAUDE.md, "Toolchain and conventions").

set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

# The gate: exactly what CI runs, in the order that fails fastest.
check: fmt-check clippy deny test doc

# Rewrite formatting in place.
fmt:
    cargo fmt --all

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all --check

# Lint every target and feature, warnings denied.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# The pure-Rust dependency guard. See deny.toml and docs/adr/0003-dependencies.md.
deny:
    cargo deny check

# Run every test. Uses nextest when installed, plus doctests, which nextest does not run.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run --workspace --all-features
        cargo test --workspace --all-features --doc
    else
        cargo test --workspace --all-features
    fi

# Build the docs, warnings denied: a broken intra-doc link is a broken reference to an invariant.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

# Run the benchmarks. Not a gate, but kept runnable at every phase; see docs/bench/.
bench *ARGS:
    cargo run --release -p esker-cli -- bench {{ARGS}}

# Run the deterministic simulator. Every failure prints a seed; re-run with it to replay.
sim *ARGS:
    cargo test -p esker-sim {{ARGS}}
