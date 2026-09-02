# Changelog

## [Unreleased] — 2026-09-02 COUNT(*) memoization + zero-warning clippy pass

### Performance

- **`SELECT COUNT(*) FROM t` 2.5 µs → 92 ns (27× faster than SQLite)**:
  bare `COUNT(*)` (no WHERE / GROUP BY / DISTINCT) now hits a dedicated
  `FastPath::CountStar` that skips executor dispatch entirely, and the
  answer is memoized per table, keyed by a monotonic `write_epoch`.
  Every mutating statement bumps the epoch ONCE at the top of
  `Database::execute` (the gateway for DML/DDL/transaction control/fast
  inserts), and prepared-statement DML bumps it in its own merge-back
  path — so a cached (epoch, count) pair can never outlive the state it
  describes (writers are `&mut self`, exclusive with `&self` readers;
  SQLite semantics preserved for uncommitted rows, ROLLBACK, and
  savepoint rollbacks). 10 new tests (`tests/count_cache.rs`) verify
  every invalidation path: per-insert counts, uncommitted rows,
  ROLLBACK, ROLLBACK TO SAVEPOINT, prepared-statement DML, DROP +
  re-CREATE, cross-table independence, WHERE-range counts, file reopen,
  and DISTINCT/GROUP BY correctness.
- **`bench_full_vs_sqlite`: 18/18 rows now outright wins** (was 16
  wins / 1 tie / 1 loss): COUNT(*) 32.9×, multi-VALUES 100-row batches
  1.17× (quiet-machine probes: fixed per-batch cost 0.77 µs vs SQLite's
  1.37 µs, per-row 311 vs 427 ns).

### Compat (C ABI)

- **`mode=ro` / `SQLITE_OPEN_READONLY` is now enforced**: a read-only
  connection rejects database-mutating statements at PREPARE with
  `SQLITE_READONLY` ("attempt to write a readonly database"); SELECTs
  and transaction control are unaffected (SQLite readonly-connection
  semantics; previously the parsed flag was advisory-only). Verified
  end-to-end through real sqlx: `SqliteConnectOptions::read_only(true)`
  serves reads and rejects writes (new interop test #11).
- `Conn` is now explicitly `Send + Sync` (unsafe impls with SAFETY
  comments: raw pointers only appear in mutex-guarded C callback slots;
  database state lives behind the engine's own RwLock — the serialized
  mode SQLite's default builds provide and sqlx-sqlite's worker thread
  relies on).
- `sqlite3_exec`'s vestigial bookkeeping removed (dead initial `rc`,
  unused `first` flag); the never-constructed `StmtKind::Empty` variant
  and the dead `private_engine` field are gone; the
  `positional_counter` parameter (threaded through 44 sites, never
  read) is removed from the parameter-walker.

### Code quality (clippy: 0 warnings across all configurations)

- All 4 build configs clean: default, `--features sqlx`,
  `--no-default-features`, and `-p rustqlite-compat` — no suppressions
  added; every warning fixed at the source:
  - 3 clippy **errors** (`not_unsafe_ptr_arg_deref`): the plugin ABI's
    `api_result_text` / `api_result_blob` / `api_result_error` /
    `api_aggregate_context` are now `unsafe` with `# Safety` sections
    (the C-ABI contract they always had, now encoded in the types).
  - 37 missing `# Safety` sections in `src/ffi.rs`: every unsafe C-ABI
    function documents its pointer contract (live connection / live
    statement / valid NUL-terminated strings / valid (buf, len) pairs).
    The three pointer-free functions (`libversion`, `threadsafe`,
    `source_id`) became plain safe `extern "C" fn` — safe is safe.
  - The C trampoline table and the ffi handle/registration bridges are
    `#[cfg(feature = "extension")]`-gated: without the feature there is
    no `load_extension`, so they were dead code (mirrors SQLite's
    `SQLITE_OMIT_LOAD_EXTENSION`).
  - Style fixes throughout: `?` operator instead of match-return,
    `matches!` instead of bool match, `clamp` instead of `.min().max()`,
    `sort_by_key(Reverse)`, `Option::filter` instead of nested ifs,
    needless returns/closures/parens/trailing semicolons, type aliases
    for complex types (`VtabScanResult`, `ColumnMeta`, `UpdateHook`,
    `QueryCase`), doc-lazy-continuation rewrites, rustfmt fixes.
- Probe fixes: 3 stale engine probes updated for `Cell::decode`'s
  page_size argument; new probes `probe_count_cost` (COUNT breakdown vs
  SQLite) and `probe_mvalues_cost` (multi-VALUES fixed vs per-row cost).

## [Unreleased] — 2026-09-02 native sqlx driver + overflow pages sprint

### sqlx (native Rust driver, `features = ["sqlx"]`)

- **`src/sqlx_driver`** — sqlx-core 0.9's driver traits implemented
  directly against the engine: `rustqlite` now works with sqlx as a
  **plain library dependency** — no `libsqlite3.so`, no C ABI, no
  `[patch.crates-io]`, no C toolchain, 100% safe Rust. `Pool`,
  `query()`/`query_as()`/`query_scalar()`, `FromRow`, transactions with
  isolation levels, `fetch()` streaming, pool options, `?` and `:name`
  binds, `RETURNING`, and multi-statement raw scripts all work through
  the unmodified `sqlx` facade. URL scheme:
  `rustqlite://app.db?mode=rwc`, `rustqlite://:memory:?cache=shared`.
- **Snapshot isolation between connections (SQLite semantics)**: while a
  transaction with uncommitted writes is open, other connections' reads
  WAIT (never observe half-applied state — verified by
  `examples/probe_dirty_read.rs`), read-only transactions never block
  readers, blocked statements fail with `SQLITE_BUSY` after the
  busy timeout (default 5 s, sqlx-sqlite parity), and a dropped
  connection rolls back whatever engine-level transaction it left open
  (sqlx-managed or raw `BEGIN`) so one connection can never wedge the
  pool.
- **Driver-vs-driver benchmark** (`examples/bench_sqlx_native.rs`):
  11/11 scenarios at parity or faster than sqlx-sqlite through the
  identical sqlx API — 1.5× INSERT/UPDATE, 2.7× PK lookups, 2.6×
  filtered scans, 2.8× GROUP BY, 3.7× transactions, **18.4× `fetch()`
  streaming**, 5.1× 8-task single-pool concurrency, 2.8× 8-connection
  reads, 2.0× 1-writer/7-readers.
- **27 new tests** (`tests/sqlx_driver.rs`): pool + URL parsing, typed
  binds and fetch, transactions with isolation and rollback, concurrent
  transactions serialize correctly, multiple concurrent read
  transactions, readonly transactions don't block readers, writer
  wake-after-foreign-commit, dropped-connection cleanup (sqlx and raw
  scripts), busy-timeout behavior (instant fail, timeout, then BUSY),
  constraint error mapping, file-backed pools, pragma round-trips.

### Storage

- **Overflow pages** (SQLite's overflow-chain layout): payloads larger
  than a page store a local prefix in the table-leaf cell plus a linked
  chain of `Overflow` pages for the tail. Megabyte BLOBs/TEXTs
  round-trip exactly (previously a hard "too big" error at ~page size),
  reads stream the chain without buffering, DELETEs recycle the chain's
  pages through the freelist, and corruption of chain linkage fails
  gracefully (integrity_check + db_corrupt_fuzz coverage).
- **Default page size 8192 → 4096** (SQLite's default since 3.12):
  byte-exact file-size parity with SQLite on identical workloads
  (262,144 bytes each on the 10k-row bench DB), equal-or-faster on all
  hot paths (leaf cache gets 2× the entries for the same memory).
- **Fixed: statement DML could lose rows after the first B+tree split**
  — the statement's DML merge-back propagated max-rowids but not new
  root pages, so after the first split every subsequent insert/read went
  through the STALE root: a 5000-row insert silently retained ~391 rows
  (one leaf's worth). Regression test
  `regression_statement_dml_survives_btree_splits` inserts 5000 rows via
  a prepared statement and verifies all of them survive.

### Performance

- **Fused Filter-over-Scan streaming with selective decode + LIMIT
  pushdown** (executor + statement drivers): `WHERE`-filtered table
  scans decode ONLY the predicate's columns for non-matching rows
  (matching rows materialize fully for parent operators); `LIMIT k`
  stops the walk at the k-th passing row instead of scanning the whole
  table. Streaming statements (`prepare`/`step`) resume the driver
  across batches by rowid — `fetch()` streams 18× faster than
  sqlx-sqlite.

### Tests

- 12 new overflow tests (`tests/overflow.rs`): round-trips at the
  local/spill boundary and far beyond, 1 MiB blobs, exact-byte
  verification, delete-and-reuse of chain pages, integrity check,
  corrupt-chain graceful failure.
- Corruption-fuzz hardening: random byte strikes now force a real flip
  (a strike rewriting the same byte value was a no-op that tripped the
  test's own no-op assert).

## [Unreleased] — 2026-09-02 gap-close sprint: every benchmark row at parity or faster

### Performance

- **GROUP BY 2.6× faster** (`SELECT val/100 AS bucket, COUNT(*) FROM t
  GROUP BY bucket`: 1.92 → 0.72 ms vs SQLite 1.84 ms): the streaming
  aggregate's general path now compiles GROUP-BY keys AND aggregate args
  that are arithmetic over columns/literals/params into positional
  `CompiledExpr` trees once per statement (~5-15 ns/row vs the ~60-120
  ns/row `eval_row` AST walk + name resolution). When everything
  compiles, the row decode becomes SELECTIVE (`decode_row_selective_wide`
  — full-width buffer, identity indexing, only referenced columns
  decoded), the single-key case skips the key-slice Vec entirely
  (`intern_one`), and `HashGrouper` hashes with FxHash (~5-10 ns vs
  SipHash's ~25-40 ns per key). `COUNT(*)`'s placeholder argument is a
  shared static — zero per-row Value construction.
- **Point lookup by rowid 1.2× faster than SQLite** (246 vs 350 µs per
  1000 ops; was 375-402 µs, ~1.08× SLOWER):
  - **Bucket-keyed 2-way set-associative leaf cache**: table-leaf hint
    slots are keyed by `rowid >> 8` (two slots per bucket). One B+tree
    descent now primes a whole 256-rowid bucket, so a first-visit sweep
    over sequential rowids (fixed-parameter OLTP loops, the bench's
    `WHERE id = ?` cycling pattern) descends ~once per leaf pair instead
    of once per rowid. The fill policy refreshes the same-page entry,
    prefers empty/stale slots, and evicts the leaf farther from the probe
    — a bucket straddling a leaf boundary keeps BOTH leaves resident.
  - **Cursor-ix cell bias** (SQLite's `pCur->ix` technique): every
    table-leaf and index-leaf search probes the remembered cell index
    first, then falls back to an exact bracketed search. Sequential
    rowid/key access resolves in 1-2 probes instead of log2(cells)
    (~8-10 for a 290-cell leaf). The bias rides the existing epoch-checked
    hint slots, so staleness remains impossible.
  - **`maps_populated` atomic flag**: fast-path point lookups skip the
    bookkeeping-maps read-lock entirely while no B+tree split has moved a
    root (the overwhelmingly common case). Writers refresh the flag at
    every attach site — `query` is `&self`, writers `&mut self`, so a
    stale `false` is impossible.
- **First-query-after-bulk-write latency 287 → 41 µs**: mimalloc's
  delayed-free queue wakes on the first allocating READ after a bulk
  write transaction (200-400 µs, measured in
  `examples/probe_drain_cold.rs`). The engine now estimates freed blocks
  per write statement (rows × 6 + write-ops × 150 + AST teardown) and, at
  write-burst completion (COMMIT / auto-commit / DDL — mid-transaction
  drains are useless, the remaining statements re-arm the queue), drains
  the wake with a 512-allocation small-class tap (~170 µs, amortized
  inside the multi-ms transaction). Once per process; a read-side
  safety net covers DML-via-RETURNING bursts.
- **UPDATE payload-patch fast path**: when the table has no NOT NULL /
  CHECK / enforced-FK constraints, no RETURNING, and every SET
  compiles, the new payload is built by copying the OLD payload bytes
  and patching only the assigned columns' byte regions
  (`row_column_regions_into` walks the encoded layout; each new value
  must keep its encoded size). Skips the full-row decode and full-row
  re-encode (~60-90 ns/row). Any mismatch (size change, missing column,
  decode error) falls back to the generic path — identical semantics.
  8 new differential cases (same-size REALs, size-changing TEXT, int
  size-class boundaries, multi-assign, NULL transitions).

### Fixed

- **`--no-default-features` build**: `ffi.rs`'s `sqlite3_load_extension`
  bridge called `Database::load_extension` unconditionally, which is
  `#[cfg(feature = "extension")]` — the no-extension build failed to
  compile. It now returns a clean "extension loading is disabled" error
  instead (mirroring SQLite's SQLITE_OMIT_LOAD_EXTENSION behavior).

### sqlx / sea-orm

- All four in-repo interop suites re-verified on this build:
  `sqlx-interop` (pool, binds, error mapping, raw_sql, 8-connection
  concurrent pool, blob+NULL round-trips), `sea_orm_interop` (CRUD,
  transactions, UNIQUE error code 2067 propagation), `sea_orm_relations`
  (junction models, Linked chains, grouped loading, paginator), and
  `migrate_interop` (sqlx::migrate! fresh apply, idempotent re-run,
  atomic rollback of failing migrations, cross-pool visibility).

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
