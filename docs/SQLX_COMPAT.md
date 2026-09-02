# Using rustqlite with sqlx and sea-orm

There are **two supported integration paths** — pick by how your app is
wired:

| | **A. Native Rust driver** (recommended for sqlx apps) | **B. Drop-in `libsqlite3`** (for sea-orm & existing binaries) |
|---|---|---|
| Add to your app | `rustqlite = { features = ["sqlx"] }` | one `[patch.crates-io]` line |
| C toolchain / FFI | none — 100% safe Rust | builds + links `libsqlite3.so` |
| Works with | sqlx 0.9 (unmodified, generic API) | sqlx 0.9 **and sea-orm 2.0** (unmodified) |
| Perf vs sqlx-sqlite | 1.5–2.8× (18× on `fetch()` streams) | ≈ rusqlite-level (C ABI + worker thread) |
| Doc | [`src/sqlx_driver` module docs](../src/sqlx_driver/mod.rs) | this page, below |

## Path A — native Rust driver (`features = ["sqlx"]`)

No `libsqlite3.so`, no `[patch.crates-io]`, no C compiler. The driver
implements sqlx-core's `Database` traits directly against the engine, so
`Pool`, `query()`/`query_as()`/`query_scalar()`, `FromRow`, transactions
with isolation levels, `fetch()` streaming, and pool options all work
through the **unmodified `sqlx` facade**.

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "macros"] }
rustqlite = { path = "../rust-sql", features = ["sqlx"] }
```

```rust
use rustqlite::sqlx_driver::RustqlitePool;

let pool = RustqlitePool::connect("rustqlite://app.db?mode=rwc").await?;

sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
    .execute(&pool).await?;
let v: Option<String> = sqlx::query_scalar("SELECT v FROM t WHERE id = ?")
    .bind(1).fetch_optional(&pool).await?.flatten();
```

Connection URLs: `rustqlite::memory:`, `rustqlite://:memory:?cache=shared`
(process-wide shared memory DB), `rustqlite://app.db` with `mode=ro|rw|rwc`
and `immutable` options — shaped like sqlx-sqlite's `sqlite://` URLs.

Isolation semantics are SQLite's: readers never observe another
connection's uncommitted writes (they wait, then `SQLITE_BUSY` after the
busy timeout); read-only transactions never block readers; a dropped
connection rolls back whatever transaction it left open. See the module
docs and `tests/sqlx_driver.rs` (27 tests) for the full contract.

**sea-orm note**: sea-orm's connection layer hardcodes the three sqlx
backends it ships (sqlite/postgres/mysql), so a custom sqlx backend
requires a sea-orm fork. For sea-orm apps use **Path B**, which needs
zero changes to sea-orm.

## Path B — drop-in `libsqlite3` (sea-orm, unmodified everything)

The rustqlite engine ships a **drop-in `libsqlite3-sys` replacement** backed by
its own engine (see `compat/`). Because sqlx's SQLite driver talks to
`libsqlite3-sys` — and sea-orm's SQLite backend sits on sqlx — **unmodified
crates.io releases of sqlx (0.9) and sea-orm (2.0) run on rustqlite** via a
Cargo `[patch]`. No fork, no code changes in either ecosystem crate.

```
your app ── sea-orm 2.0 (crates.io, unmodified)
              └── sqlx 0.9 (crates.io, unmodified)
                    └── libsqlite3-sys  ──[patch.crates-io]──►  rust-sql/compat/libsqlite3-sys
                                                                  └── links libsqlite3.so
                                                                        └── rustqlite-compat (124 sqlite3_* symbols)
                                                                              └── rustqlite engine
```

### Path B, Step 1 — build the compat library

From the rust-sql repo root:

```bash
cargo build --release -p rustqlite-compat
# → target/release/libsqlite3.so
```

### Path B, Step 2 — patch it into your project

In your app's `Cargo.toml`:

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
sqlx = { version = "0.9", default-features = false, features = [
    "runtime-tokio",
    "sqlite-unbundled",   # IMPORTANT: uses libsqlite3-sys instead of bundling C SQLite
    "macros",
] }
sea-orm = { version = "2.0", features = ["sqlx-sqlite", "runtime-tokio"] }

[patch.crates-io]
libsqlite3-sys = { path = "../rust-sql/compat/libsqlite3-sys" }
```

`sqlite-unbundled` is the key sqlx feature: it links the system
`libsqlite3.so` — which the patch redirects to the rustqlite-backed build.

The compat `build.rs` resolves the library location in this order:

1. `RUSTQLITE_LIB_DIR` env var (absolute path containing `libsqlite3.so`),
2. `<rust-sql repo>/target/release` (relative to the patch path).

So either export `RUSTQLITE_LIB_DIR=/path/to/rust-sql/target/release`, or
rely on the default relative resolution. An rpath is baked into your binary,
so the `.so` is found at runtime with no `LD_LIBRARY_PATH` needed.

### Path B, Step 3 — just use sqlx / sea-orm normally

```rust
use sqlx::sqlite::SqlitePoolOptions;

let opts = sqlx::sqlite::SqliteConnectOptions::new()
    .filename("app.db")            // or ":memory:"
    .create_if_missing(true);
let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;

sqlx::query("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)")
    .execute(&pool).await?;

// Typed binds, aggregates, transactions, RETURNING, multi-statement
// raw_sql scripts, concurrent pool connections — all behave as on SQLite.
let id: i64 = sqlx::query_scalar("INSERT INTO users (name, email) VALUES (?, ?) RETURNING id")
    .bind("Ada").bind("ada@example.com")
    .fetch_one(&pool).await?;
```

sea-orm works unchanged on top:

```rust
let db = Database::connect("sqlite://app.db?mode=rwc").await?;
// DeriveEntityModel, Schema::create_table_from_entity, insert with
// auto-generated PK, find/filter/order/update/delete, transactions with
// rollback, commit paths — all against the rustqlite engine.
```

## What is verified (see `sqlx-interop/` as a template)

The `sqlx-interop/` workspace (a separate in-repo workspace that patches
the crates.io releases) runs both stacks end-to-end against the engine,
and `tests/sqlx_driver.rs` covers the native driver (27 tests):

**sqlx 0.9** — pool + connect; DDL; typed INSERT/SELECT binds;
`last_insert_rowid`; `query_as` row mapping; aggregates; explicit
BEGIN/COMMIT; ROLLBACK; constraint error mapping (duplicate →
`DatabaseErrorKind::UniqueViolation` with message `UNIQUE constraint failed:
uniq.email`; NULL → `NotNullViolation` with `NOT NULL constraint failed:
uniq.email`); `raw_sql` multi-statement scripts; 8 concurrent pool
connections (cross-connection transaction serialization); blob + NULL
round-trips.

**sea-orm 2.0** — entity derives; `Schema::create_table` from entities
(including `#[sea_orm(unique)]` constraints); insert with auto PK;
`find_by_id`; filter + order; update; delete; transaction + rollback; commit;
duplicate-insert error propagation with the SQLite-exact message and code
2067.

## Behavioral notes

- **Foreign keys**: upstream `sqlite3` CLI defaults `PRAGMA foreign_keys` to
  OFF; rusqlite's *bundled* build and sqlx's `SqliteConnectOptions` default
  it ON. The engine honors the pragma either way — set it explicitly when
  you care.
- **Error text**: constraint errors are byte-identical to SQLite's
  `sqlite3_errmsg` output, and result codes carry SQLite's extended codes
  (`SQLITE_CONSTRAINT_UNIQUE` etc.). sqlx's `error_kind()` classification
  relies on exactly these.
- **WAL**: the engine uses its own WAL; `PRAGMA journal_mode` accepts the
  SQLite spellings (the compat layer returns the mode row).
- **Version string**: `sqlite3_libversion()` reports `3.50.4` so
  version-gated ecosystem code takes the modern path.
- **One engine per file path per process** (shared across pool
  connections), mirroring SQLite's shared-cache file locking via a
  transaction slot: a second writer gets `SQLITE_BUSY` → waits up to
  `busy_timeout` → retries.
