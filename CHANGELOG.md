# Changelog

## [Unreleased] — sqlx/sea-orm drop-in compatibility, full UPDATE constraint semantics

### Added

- **`compat/` — drop-in SQLite C ABI replacement** (see `compat/README.md`):
  - `rustqlite-compat`: 124 real `sqlite3_*` symbols on the engine
    (open_v2 with URI flags, prepare_v2/v3 with pzTail, full step state
    machine, binds, column/value objects, changes bookkeeping,
    busy_timeout with cross-connection tx serialization, hooks,
    create_function_v2/collations, load_extension). Builds
    `libsqlite3.so`/`.a`.
  - `libsqlite3-sys`: drop-in crates.io `libsqlite3-sys` 0.30.x
    replacement (vendored bindings + link/rpath build script), consumed
    from external workspaces via `[patch.crates-io]`.
  - 30 raw-ABI conformance tests (`compat/rustqlite-compat/tests/compat_abi.rs`).
  - `docs/SQLX_COMPAT.md`: end-to-end guide — **unmodified sqlx 0.9 and
    sea-orm 2.0 run on the rustqlite engine** (verified externally:
    pool, DDL, binds, last_insert_rowid, query_as, transactions,
    UniqueViolation/NotNullViolation mapping on SQLite-exact messages,
    raw_sql multi-statement, 8-connection pools, full sea-orm CRUD +
    error propagation).

- **`Error::Constraint`** — SQLite-exact, prefix-free constraint messages
  (`UNIQUE constraint failed: t.c`, `NOT NULL constraint failed: t.c`,
  `CHECK constraint failed: t`, `FOREIGN KEY constraint failed`,
  `datatype mismatch`); FK runtime messages now match SQLite byte-for-byte.

- **Full UPDATE constraint semantics** (all three execution paths —
  general, streaming, `UPDATE ...FROM`):
  - **UNIQUE-index enforcement with SQLite's sequential semantics**: a
    write-set simulation walks rows in scan order, subtracting vacated
    keys and adding claimed keys, so `UPDATE t SET v = v - 1` on
    1,2,3 succeeds while swaps conflict — exactly like SQLite. Statement
    aborts atomically (checks run before any B+tree modification).
    Collation-aware: NOCASE/RTRIM fold into probe keys.
  - **`UPDATE OR IGNORE` / `OR REPLACE`** plumbed end-to-end (planner →
    executor): IGNORE skips conflicting rows (changes() counts only
    applied rows; RETURNING output filtered), REPLACE deletes the
    conflicting holder row (table + all index entries) first.
  - **Rowid-alias moves**: `UPDATE t SET id = X` now moves the row (cell
    key change, delete + reinsert with every index entry re-keyed) and
    enforces rowid uniqueness (`UNIQUE constraint failed: t.id`); NULL
    on the alias reports `datatype mismatch` / SQLITE_MISMATCH.
  - **FK child-side checks + NULL-alias checks** added to the general and
    `UPDATE ...FROM` paths (previously streaming-only).
  - Index maintenance errors are now propagated (previously discarded).

### Fixed

- Corrupt-page robustness: out-of-range cell pointers in index-interior
  binary search now yield SQLITE_CORRUPT-style misses instead of a slice
  panic (found by `db_corrupt_fuzz`).
- Compat layer: `sqlite3_changes` no longer clobbered to 0 by the final
  `SQLITE_DONE` step of a RETURNING statement (per-run reporting flag).

### Tests

- `tests/pragma_introspect.rs` — 25 differential tests vs real SQLite for
  `PRAGMA table_info`/`table_xinfo`/`index_list`/`index_xinfo`/
  `foreign_key_list`, `journal_mode` result rows (type-tagged value
  equality, NULLs included).
- `tests/update_from_collate.rs` — 46 differential tests: UPDATE...FROM
  (SQLite 3.33+ semantics), NOCASE/RTRIM collations, unique violations,
  OR IGNORE / OR REPLACE, rowid moves, composite keys, ratchets, atomic
  aborts, error message shapes.

## [Plugins] — Plugin system, SQLite-style C API, streaming statements

### Added

- **Plugin system** (`src/plugin/`, guide in `PLUGINS.md`):
  - User **scalar functions** (`Database::create_function`,
    `rustqlite::plugin::ScalarFunction`) — case-insensitive dispatch,
    arity enforcement, deterministic() hint, built-in shadowing rejected.
  - User **aggregate functions** (`Database::create_aggregate`) —
    per-group state objects, empty-group xFinal semantics, mixed with
    built-ins in one SELECT, planner-integrated (GROUP BY/HAVING).
  - **Collations** (`Database::create_collation`) — built-ins `NOCASE`
    (ASCII) and `RTRIM`, user-defined sequences; honored by
    `ORDER BY … COLLATE` and comparison operators with COLLATE operands.
  - **Virtual tables** — `CREATE VIRTUAL TABLE … USING module(...)`
    with SQLite's callback protocol (xCreate/xConnect/xBestIndex
    constraint pushdown/cursor scan/xUpdate writes), persistence across
    reopen with connect-on-module-registration, DROP → xDestroy,
    integration with joins, aggregates, EXPLAIN, and streaming.
  - **Page codecs** (`Database::create_codec`, `PRAGMA codec`) —
    page-level encode/decode through the pager with header markers,
    safe refusal on wrong codec via `Database::open_with_codec`, WAL
    mutual exclusion; `XorCodec` ships as a working example.
  - **Dynamic extension loading** (`Database::load_extension`, feature
    `extension`, on by default) — `dlopen` of libraries exporting
    `rustqlite_extension_init(const rql_api*, rql_db*, char**)`;
    process-lifetime API table; working example plugins in **C, C++,
    Zig, and Rust** under `plugins/` (built by `tests/build_plugins.sh`).
  - **C ABI header** (`include/rustqlite_ext.h`) — one header for all
    four languages.

- **SQLite-shaped C API** (`src/ffi.rs`, `rustqlite_*` exports):
  `open`/`open_in_memory`/`close` (refuses live statements),
  `exec`, `prepare_v2` (+ pzTail), `step` (ROW/DONE), `finalize`,
  `reset`, `clear_bindings`, `bind_int64/int/double/text/blob/null`
  (1-based, with destructor conventions), `bind_parameter_count/index`,
  `column_count/name/type/int64/double/text/blob/bytes` (0-based),
  `changes`, `total_changes`, `last_insert_rowid`, `errcode`/`errmsg`,
  `libversion`/`source_id`/`threadsafe`, `create_function`,
  `create_collation`, `create_module`, `load_extension`.

- **Prepared statements** (`src/statement.rs`):
  `Database::prepare` → `Statement::bind/bind_named/bind_all/step/
  reset/clear_bindings/query_all/raw_execute/finalize`, 1-based binds
  (SQLite convention), named parameters with or without sigils,
  `parameter_count`/`parameter_names`, `column_*` accessors, `changes()`.
  **Streaming drivers** for Scan / RowidRange / Filter / Project /
  Limit / virtual-table scans: resumable batch-pulled row sources with
  early termination — large SELECTs never materialize. DML without
  RETURNING surfaces DONE (not the engine's internal change-count row).
  Concurrent readers share the connection read-lock through statements.

### Changed

- `Plan::Scan` execution routes through virtual-table drivers when the
  catalog table is a vtab; DML (INSERT/UPDATE/DELETE) routes to xUpdate.
- `exec_filter` passes its predicate into the vtab scan (best_index
  constraint extraction; unhandled conjuncts stay as residual filters).
- `Aggregate` fast paths (#0 COUNT-cells, #1 streaming scan) fall through
  to the general path for virtual tables.
- Planner (`is_aggregate_fn`/`is_aggregate_call`) recognizes registered
  plugin aggregates through the statement plugin scope.
- `get_or_cache_stmt` installs the plugin scope when planning (registry
  visibility); zero-plugin databases skip it via an atomic fast path
  (`has_plugins`), keeping the no-plugin hot path at one relaxed load.
- The fast INSERT byte-scanner declines virtual tables (xUpdate path).
- Pager: codec hooks in `get_page` (decode on main-file read) and the
  DELETE-mode flush (encode on write), codec marker in header bytes
  72..100, `set_codec`/`codec_name`/`required_codec`, WAL guard.

### Fixed

- `SELECT count(*)` / aggregates over virtual tables no longer count
  B+tree cells of the schema root (fast paths now vtab-aware).
- vtab residual-conjunct mapping: conjuncts extracted as constraints but
  NOT marked handled by best_index correctly remain engine-applied
  filters (previously the whole predicate could be dropped when the
  module handled only part of it).
- Statement `column_count`/`column_name` work before the first step
  (columns precomputed from the driver at prepare time, like SQLite).
- Named parameter binding keyed by the original sigil form (the engine's
  HashMap key), matching bare or sigil'd input names.

### Testing

- `tests/plugins.rs` (25): scalar/aggregate/collation/vtab (read +
  writable, persistence + reconnect-on-module-registration, readonly
  rejection, aggregates, joins, streaming) / codec round-trip + wrong-key
  + PRAGMA forms / introspection.
- `tests/statement_api.rs` (14): step/bind/reset semantics, streaming
  scan/range/filter/limit drivers, named parameters, DML through
  statements, DDL/txn rejection, aggregates, concurrent readers.
- `tests/ffi.rs` (9): the full `rustqlite_*` family plus loading the
  C, C++, Zig, and Rust example extensions end-to-end (functions,
  aggregates, collations, vtabs including writes).
- `tests/build_plugins.sh` builds all four example plugins.
- Full matrix: 312 tests green (was 264); head-to-head benchmarks
  re-verified at parity-or-faster on all workloads.
