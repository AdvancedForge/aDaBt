# Semver and format-compatibility promise

**Version gate:** `superblock` format number (single `u32`) refuses newer database outright. Migration enumerated in code (`HeapStore::migrate`, legacy-identity adoption). Catalog (`catalog.adabt`) versioned separately (currently v4: `delta_encoding` + `thread_per_core` persisted), loss recoverable: unreadable catalog rebuilds from WAL with every record intact (tested end-to-end `catalog_persistence`).

**Disk semver:** `MAJOR` bumps on `superblock` format change; `MINOR` on `catalog` extension (v3→v4 backward-compatible: v3 catalog accepted, flags default `true`/`false`). WAL `FORMAT_VERSION` independent. Record codec `FORMAT_VERSION=1` stable; new field types add `FieldType` variants with `fixed_width` handling, never reinterpret existing bytes.

**CI gate:** `cargo test --workspace` includes `catalog_persistence`, `superblock` refusal, and `migrate` proof. Format-breaking change fails CI unless `superblock` version gate and migration land together (see `docs/roadmap.md` Stage 8 finish test).
