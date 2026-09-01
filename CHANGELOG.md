# Changelog

## [Unreleased] — Plugin system, SQLite-style C API, streaming statements

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
