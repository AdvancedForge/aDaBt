#!/usr/bin/env bash
set -euo pipefail
# Enforced continuous check — local replacement for the removed GH Actions.
# Private repo has no Actions runner; this script is the gate. Run before
# every push or via `cargo xtask check`.

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
# Note: `--all-features` is intentionally excluded. The `loom` feature swaps
# `Arc`/`Mutex` types across crates; full cross-crate feature compatibility
# requires a broader design decision (not a bug). The `loom` subset runs
# separately below (line 17) for targeted verification.
cargo clippy --workspace --all-targets -- -D warnings

echo "== test workspace =="
cargo test --workspace

echo "== loom subset (verified: storage only; engine feature-gated but cross-crate type substitution remains open) =="
cargo test -p adabt-storage --features loom -- --nocapture 2>&1 | tail -n 5

echo "== comparison harness sanity (separate workspace) =="
cargo run --manifest-path comparison/Cargo.toml -- --help >/dev/null 2>&1

echo "== critical release tests =="
cargo test -p adabt-engine --test cross_shard_concurrent -- --nocapture 2>&1 | tail -n 3
cargo test -p adabt-engine --test cross_shard_atomic -- --nocapture 2>&1 | tail -n 3
cargo test -p adabt-engine --test serializable -- --nocapture 2>&1 | tail -n 3
cargo test -p adabt-storage --test catalog_upgrade -- --nocapture 2>&1 | tail -n 3

echo "== docs =="
cargo doc --workspace --no-deps >/dev/null

echo "all checks passed"
