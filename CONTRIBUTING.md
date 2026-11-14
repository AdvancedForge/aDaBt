# Contributing

aDaBt is an experimental research project at `0.1.0-alpha.1`. Read `docs/limitations.md` and `docs/roadmap.md` ("What's next") before proposing work.

Before contributing:
- Run `scripts/check.sh` locally (`git config core.hooksPath .githooks` if you want pre-push checks).
- Ensure `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass.
- The `loom` model checks run as their own step (`--features loom --lib loom_tests`): the feature swaps `Arc`/`Mutex` for loom's, which only work inside `loom::model`.
- API and on-disk format stability are not guaranteed until beta (`docs/semver.md`).
- `docs/history/` is a record of past milestones; don't update it — put current facts in the other docs.
