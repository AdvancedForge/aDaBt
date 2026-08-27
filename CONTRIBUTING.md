# Contributing

This is a private research repository moving toward public `0.1.0-alpha.1`. Contributions are welcome once the repository is public.

Before contributing:
- Run `scripts/check.sh` locally (`git config core.hooksPath .githooks` if you want pre-push checks).
- Ensure `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass.
- Note the `loom` subset (`--features loom`) is a separate, slower verification path due to `Arc`/`Mutex` type conflicts when combined with `--all-features`.
- The release target is `0.1.0-alpha.1`; API and format stability are not guaranteed until beta.
