# sqlx-interop — unmodified sqlx 0.9 + sea-orm 2.0 on the rustqlite engine

This is a **separate workspace** (deliberately outside the engine's
workspace: a Cargo workspace cannot `[patch.crates-io]` a crate that one of
its members path-depends on). It proves, end-to-end, that the crates.io
releases of **sqlx** and **sea-orm** run unchanged on rustqlite via the
`compat/` C ABI layer — no forks, no patches to either ecosystem crate.

```
sqlx-interop ── sea-orm 2.0.2 (crates.io, unmodified)
                 └── sqlx 0.9.0 (crates.io, unmodified)
                       └── libsqlite3-sys 0.30.1 ──[patch.crates-io]──► ../compat/libsqlite3-sys
                                                                  └── links ../target/release/libsqlite3.so
                                                                        └── rustqlite engine
```

## Run

```bash
# 1. Build the compat shared library (repo root):
cargo build --release -p rustqlite-compat

# 2. Run any of the four suites (this directory):
RUSTQLITE_LIB_DIR=../target/release cargo run --release --bin sqlx-interop        # sqlx core
RUSTQLITE_LIB_DIR=../target/release cargo run --release --bin sea_orm_interop    # sea-orm CRUD
RUSTQLITE_LIB_DIR=../target/release cargo run --release --bin sea_orm_relations  # sea-orm relations
RUSTQLITE_LIB_DIR=../target/release cargo run --release --bin migrate_interop    # sqlx::migrate!
```

(Without `RUSTQLITE_LIB_DIR`, the build script falls back to the repo's
`target/release` via the patch's relative path.)

## What each suite verifies

| Binary | Coverage |
|---|---|
| `sqlx-interop` | pool + connect, DDL, typed INSERT/SELECT binds, `last_insert_rowid`, `query_as` mapping, aggregates, BEGIN/COMMIT, ROLLBACK, constraint error mapping (`UniqueViolation` / `NotNullViolation` on SQLite-exact messages), `raw_sql` multi-statement scripts, 8 concurrent pool connections (cross-connection tx serialization), blob + NULL round-trips |
| `sea_orm_interop` | `DeriveEntityModel` entities, `Schema::create_table` from entities, insert with auto-generated PK, `find_by_id`, filter + order, update, delete, transaction rollback + commit, duplicate-insert error propagation (UNIQUE violation with the exact SQLite message) |
| `sea_orm_relations` | junction-table entities (`#[sea_orm::model]` + `has_many, via`), `find_also_related` two-hop LEFT JOINs (with NULL rows), `Linked::find_linked` INNER JOIN chains, `find_with_related` grouped loading, the paginator (COUNT + LIMIT/OFFSET pages), relation counts with filters |
| `migrate_interop` | `sqlx::migrate!` fresh apply, idempotent re-run, `_sqlx_migrations` bookkeeping, `pragma_table_info`, atomic rollback of failing migrations, multi-pool schema visibility |

## Performance & concurrency vs SQLite

Through this C-ABI layer throughput is ≈ rusqlite-level (the
step/bind/column protocol and FFI boundary dominate), with SQLite-exact
concurrency semantics (cross-connection BUSY-then-retry, snapshot
isolation — the suites above verify both). The engine underneath beats
SQLite on every head-to-head row — performance (1.4–2.4× inserts,
2.0–4.6× reads, 2.7× GROUP BY), resource consumption (byte-exact file
size, 0.94× peak RSS), and concurrency (8.3× concurrent reads,
5.7–8.2× intra-statement parallel aggregates on 1M rows; see
`BENCHMARKS.md` [9]). For the engine's full concurrency envelope without
the FFI hop, use the native Rust driver (`features = ["sqlx"]`,
`docs/SQLX_COMPAT.md` Path A): 1.5–2.8× sqlx-sqlite on mixed workloads,
5.1× on 8-task pools, 18.4× on `fetch()` streams.
