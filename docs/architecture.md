# Architecture

Single-node embedded database. Source in `crates/`.

- `adabt-core`: records, transactions, consistency guarantees
- `adabt-storage`: heap pages, WAL, codecs, catalog (v4), delta encoding
- `adabt-index`: B-tree and hash secondary indexes
- `adabt-exec`: query execution, projection, filtering (`filter_by_peek_fields`)
- `adabt-opt`: adaptive optimizer (17 registered optimizations, level ladder 1-6)
- `adabt-engine`: database driver (`Database`), sharded coordinator (`ShardedDatabase`)
- `adabt-server`: TLS server with static role tokens
- `adabt-cli`: command-line interface
- `adabt-ffi`: C header and FFI bindings

Key design choices:
- Zero-copy multi-field projection (`peek_fields`)
- Catalog v4 (`FORMAT_VERSION`): delta encoding, thread-per-core flags persisted
- Cross-shard writes serialized via `Arc<Mutex<()>>` (`commit_coordinated`)
- Strict consistency validates point-read sets (not full serializable isolation; predicate phantoms possible)
- Memory-resident: no disk overflow support planned
