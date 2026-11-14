# Contributing

aDaBt is an experimental research project at `0.1.0-alpha.1`. Read `docs/limitations.md` and `docs/roadmap.md` ("What's next") before proposing work.

Before contributing:
- Run `scripts/check.sh` locally (`git config core.hooksPath .githooks` if you want pre-push checks).
- Ensure `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass.
- Note the `loom` subset (`--features loom`) is a separate, slower verification path due to `Arc`/`Mutex` type conflicts when combined with `--all-features`.
- API and on-disk format stability are not guaranteed until beta (`docs/semver.md`).
- `docs/history/` is a record of past milestones; don't update it — put current facts in the other docs.
