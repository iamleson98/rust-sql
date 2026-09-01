# rustqlite SQLite-compatibility layer

`compat/` makes the rustqlite engine a **drop-in replacement for SQLite at the
C ABI level**. Unmodified programs that link against `libsqlite3` — including
the **sqlx** and **sea-orm** crates from crates.io — can run on the rustqlite
engine with zero code changes.

It contains two crates:

| Crate | What it is |
|---|---|
| `rustqlite-compat` | Exports the real `sqlite3_*` symbols (124 of them) implemented on the rustqlite engine. Builds `libsqlite3.so` (cdylib), `libsqlite3.a` (staticlib) and an rlib for in-workspace tests. |
| `libsqlite3-sys` | Drop-in replacement for the crates.io `libsqlite3-sys` (0.30.x) crate: vendored upstream bindings + `build.rs` that links the rustqlite-backed `libsqlite3.so` with an rpath. Consumed from a **separate workspace** via `[patch.crates-io]`. |

## How the C ABI surface maps to the engine

- `sqlite3_open_v2` — URI flags (`mode=memory`, `cache=shared`, `rw`/`ro`/`create`). One
  engine per file path per process (`Arc<Engine{RwLock<Database>}>`); `:memory:` is
  private per connection; `file:x?mode=memory&cache=shared` is a shared named memory DB.
- `sqlite3_prepare_v2` / `prepare_v3` — real `pzTail` multi-statement scanning, prepare-time
  column names (sqlx reads `column_count`/`column_name` before the first step), DML
  `RETURNING` static names from the AST.
- `sqlite3_step` — full state machine: SELECT/DML through the engine's streaming
  `Statement` (row-at-a-time, resumable); DDL / transactions / pragma-writes through the
  one-shot path; pragma-reads through the query path. Extended result codes,
  `errcode`/`errmsg` persistence until `reset` (SQLite semantics).
- `sqlite3_bind_*` — positional and named parameters (`:name`, `@name`, `$var`),
  1-based indices, text/blob with destructor conventions.
- `sqlite3_column_*`, `sqlite3_column_value` / `sqlite3_value_*` — the
  `Box<Value>` protocol for protected value objects, `value_dup`/`value_free`.
- Bookkeeping — `changes`, `changes64`, `total_changes`, `last_insert_rowid`,
  `get_autocommit`, `busy_timeout` (with cross-connection transaction
  serialization: `tx_owner` + `await_tx_slot`, so a second writer gets
  `SQLITE_BUSY` → waits → retries, like SQLite's file locking).
- Hooks — commit / rollback / update hooks, progress handler.
- Extensability — `create_function_v2`, `create_collation` (via the engine's
  plugin registry), `load_extension`.
- Error mapping — SQLite-exact messages (`UNIQUE constraint failed: t.c`,
  `NOT NULL constraint failed: t.c`, `CHECK constraint failed: t`,
  `FOREIGN KEY constraint failed`, `datatype mismatch`) and extended codes
  (`SQLITE_CONSTRAINT_UNIQUE` 2067, `SQLITE_CONSTRAINT_NOTNULL` 1299,
  `SQLITE_CONSTRAINT_FOREIGNKEY` 787, `SQLITE_CONSTRAINT_CHECK` 275,
  `SQLITE_MISMATCH` 20, ...). ORMs (sqlx, sea-orm) pattern-match these bytes.

## Building

```bash
# From the rust-sql workspace root — produces target/release/libsqlite3.so
cargo build --release -p rustqlite-compat
```

`compat/libsqlite3-sys` is deliberately **excluded** from this workspace: it is
meant to be consumed from the *consumer's* workspace via `[patch.crates-io]`,
where its `build.rs` links `libsqlite3.so` (resolving
`RUSTQLITE_LIB_DIR`, then `<repo>/target/release`) and bakes an rpath.

## Conformance tests

`compat/rustqlite-compat/tests/compat_abi.rs` — 30 tests that drive the raw
C ABI exactly the way C programs and sqlx's worker thread do: lifecycle,
libversion shape, prepare-time column names, DML-without-RETURNING zero
columns, bind/step/reset lifecycles, named parameters, multi-statement tails
(semicolons inside strings included), changes/last_insert_rowid, transaction
autocommit states, extended constraint codes + errmsg byte-exactness, value
objects, shared-cache memory URIs, cross-connection BUSY-then-succeed,
failed-open reporting, pragmas via prepared statements — plus UPDATE
semantics: atomic statement abort, `OR IGNORE` + `changes()`, rowid moves
(`UPDATE t SET id = X`), `datatype mismatch` (SQLITE_MISMATCH) for NULL on
the rowid alias, FK extended codes, collated (NOCASE) unique violations, and
RETURNING rows + changes.

## sqlx / sea-orm

See `docs/SQLX_COMPAT.md` for the end-to-end guide (sqlx 0.9 and sea-orm 2.0,
both unmodified, running on the rustqlite engine).
