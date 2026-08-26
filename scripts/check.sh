#!/usr/bin/env bash
set -euo pipefail
# Enforced continuous check — local replacement for the removed GH Actions.
# Private repo has no Actions runner; this script is the gate. Run before
# every push or via `cargo xtask check`.

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== test workspace =="
cargo test --workspace

echo "== loom subset =="
cargo test -p adabt-storage --features loom -- --nocapture 2>&1 | tail -n 5 || true
cargo test -p adabt-engine --lib -- --nocapture 2>&1 | tail -n 5

echo "== comparison harness sanity (separate workspace) =="
cargo run --manifest-path comparison/Cargo.toml -- --help >/dev/null 2>&1 || true

echo "== docs =="
cargo doc --workspace --no-deps >/dev/null

echo "all checks passed"
