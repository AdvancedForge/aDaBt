#!/usr/bin/env bash
# The project's gate: everything that must pass before a push.
# Enable as a pre-push hook with `git config core.hooksPath .githooks`.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n== %s ==\n' "$1"; }

step "fmt"
cargo fmt --all -- --check

step "clippy"
# `--all-features` is deliberately absent: the `loom` feature swaps
# `Arc`/`Mutex` types across crates and is verified on its own below.
cargo clippy --workspace --all-targets -- -D warnings

step "tests"
cargo test --workspace

step "loom model checks (adabt-storage)"
# Only the `loom_tests` modules: under the feature every atomic is loom's,
# and loom refuses to run outside `loom::model`, so ordinary tests cannot
# share this build. The engine crate is not yet model-checked; see
# docs/limitations.md.
cargo test -p adabt-storage --features loom --lib loom_tests

step "comparison harness builds (separate workspace)"
cargo build --manifest-path comparison/Cargo.toml --release

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --quiet

printf '\nall checks passed\n'
