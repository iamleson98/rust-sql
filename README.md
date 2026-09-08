# rustqlite

A from-scratch embedded SQL database engine written in pure Rust — modeled after SQLite, built to beat it.

> **Status**: production-ready core. **564 tests** in the default matrix (crash / power-loss
> simulation, OOM + I/O fault injection, corruption + SQL fuzzing, differential verification
> against real SQLite, SQL Logic Tests, intra-statement parallelism equality checks).
> **Beats SQLite on every benchmark row — win or statistical parity** (see
> [Performance vs SQLite](#performance-vs-sqlite)). **Resource consumption at parity or
> better** (byte-exact file size, lower peak RSS — see
> [Resource consumption](#resource-consumption-vs-sqlite)). **Concurrency beyond SQLite's
> design envelope** on three tiers, including intra-statement parallelism SQLite cannot
> follow (see [Concurrency](#concurrency-vs-sqlite)). sqlx 0.9 native driver **and** drop-in
> C ABI verified with sea-orm 2.0. Full SQLite-style plugin system. Remaining deltas:
> [Remaining gaps](#remaining-gaps-vs-sqlite).

This README is the single source of truth for the project: architecture, the latest
performance / resource / concurrency comparisons against SQLite, the feature list, the
test methodology, and the honest ledger of what is still missing.

## Contents

- [Quick start](#quick-start)
- [Feature list](#feature-list)
- [Architecture](#architecture)
- [Performance vs SQLite](#performance-vs-sqlite)
- [Resource consumption vs SQLite](#resource-consumption-vs-sqlite)
- [Concurrency vs SQLite](#concurrency-vs-sqlite)
- [sqlx driver vs sqlx-sqlite](#sqlx-driver-vs-sqlx-sqlite)
- [SQLite file-format interop](#sqlite-file-format-interop)
- [Remaining gaps vs SQLite](#remaining-gaps-vs-sqlite)
- [Usage](#usage)
- [Testing](#testing)
- [License](#license)

## Quick start

```rust
use rustqlite::{Database, Value};

let mut db = Database::open("/tmp/my.db")?;
db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)", [])?;
db.execute("INSERT INTO users (name, age) VALUES ('Alice', 30), ('Bob', 25)", [])?;

let rows = db.query("SELECT name, age FROM users WHERE age > 28 ORDER BY age", [])?;
for row in &rows {
    println!("{}: {}", row[0], row[1]);
}
```

## Feature list

### SQL surface

- **DDL**: `CREATE TABLE`, `CREATE INDEX` (unique + partial), `CREATE VIEW`, `CREATE TRIGGER`, `DROP TABLE/INDEX/VIEW/TRIGGER`, `ALTER TABLE RENAME TO` (catalog + schema move with index/trigger attachment and FK reference rewriting), `ALTER TABLE ADD COLUMN` (with DEFAULT back-fill), `ALTER TABLE RENAME COLUMN` (rewrites the table's CREATE statement, other tables' REFERENCES clauses, indexes, triggers and views), `ALTER TABLE DROP COLUMN` (validates SQLite's restrictions and physically rewrites every row)
- **DML**: `INSERT` (with `OR REPLACE/IGNORE/FAIL/ABORT/ROLLBACK`, `VALUES` / `SELECT` / `DEFAULT VALUES` sources, `RETURNING`), `UPDATE` (with `SET`, `FROM`, `WHERE`, `RETURNING`), `DELETE` (with `WHERE`, `RETURNING`, `ORDER BY`, `LIMIT`)
- **UPSERT**: `INSERT ... ON CONFLICT (cols) DO NOTHING / DO UPDATE SET ... [WHERE ...]` with `excluded.*` references (SQLite semantics)
- **CHECK + NOT NULL constraints**: enforced on INSERT, UPDATE, and UPSERT merges (column-level and table-level)
- **FOREIGN KEY constraints**: enforced when `PRAGMA foreign_keys = ON` (default off, like SQLite) — child-side checks on INSERT/UPDATE, parent-side checks on DELETE with `ON DELETE RESTRICT / CASCADE (recursive) / SET NULL / SET DEFAULT`, composite keys, implicit-PK references, and index maintenance on cascaded deletes
- **Implicit UNIQUE indexes**: column/table-level UNIQUE and non-rowid PKs create `sqlite_autoindex_*` (actually enforced)
- **Subqueries**: uncorrelated scalar / `IN (SELECT ...)` / `EXISTS (SELECT ...)` — executed once per statement, arbitrarily nested; correlated subqueries execute per outer row with SQLite's scoping
- **Queries**: `SELECT` with `DISTINCT`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` (multi-key, `COLLATE`, NULLs-last), `LIMIT`/`OFFSET`
- **Joins**: `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, `NATURAL`, with `ON` / `USING`
- **Set operations**: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- **CTEs**: `WITH` (non-recursive) and `WITH RECURSIVE` — full execution
- **Window functions**: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `SUM() OVER (...)`, etc.
- **Aggregates**: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT`, with `DISTINCT`; percentile family (Turso extension parity): `median()`, `percentile_cont()`, `percentile_disc()`, `stddev()` / `stddev_samp()` / `stddev_pop()`
- **Expressions**: arithmetic, string concat (`||`), bitwise, `CASE WHEN`, `CAST`, `LIKE`, `GLOB`, `BETWEEN`, `IN`, `IS NULL`, `IS`, `COLLATE`, row values, `RAISE` inside triggers
- **Functions**: `ABS`, `LENGTH`, `LOWER`, `UPPER`, `TRIM`, `LTRIM`, `RTRIM`, `REPLACE`, `SUBSTR`, `INSTR`, `COALESCE`, `NULLIF`, `IIF`, `ROUND`, `RANDOM`, `HEX`, `TYPEOF`, `PRINTF`, scalar `MIN`/`MAX`, math functions, `CONCAT` / `CONCAT_WS` (SQLite 3.44+), `OCTET_LENGTH` (3.46+), `GLOB()`, `UNHEX`, `ZEROBLOB`, `SQLITE_SOURCE_ID`, and more
- **JSON1**: `json()`, `json_extract()` (incl. `$[n]`, `$.a.b`, `[#-n]` paths, unicode escapes, surrogate pairs), `json_valid()`, `json_type()`, `json_quote()`, `json_array()`, `json_object()`, `json_array_length()`, `json_insert()`, `json_replace()`, `json_set()`, `json_remove()`, `json_patch()` (RFC 7396), `json_pretty()` (3.46+), `json_error_position()` (3.47+), `json_group_array()` / `json_group_object()`
- **Date/time (full SQLite compatibility)**: `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`, `strftime()`, `timediff()` with all modifiers (`+N days/months/years`, `start of month/year/day`, `end of month`, `weekday N`, `unixepoch`, `localtime`/`utc`, `subsec`) — a faithful port of SQLite's `date.c`
- **Transactions**: `BEGIN [DEFERRED]`, `COMMIT`, `ROLLBACK`, savepoints (`SAVEPOINT` / `RELEASE` / `ROLLBACK TO`) with nested capture/rollback; auto-commit per statement
- **Pragmas**: `foreign_keys`, `page_size` (512 B–64 KiB), `parallel_scan` (intra-statement worker split), `integrity_check`, `codec`, `journal_mode` (WAL / delete), `busy_timeout`, and more — others parse and are accepted as no-ops, exactly like SQLite does for unknown pragmas

### Storage

- **Page format**: 4 KiB pages (SQLite's default since 3.12, configurable 512 B–64 KiB via `PRAGMA page_size`), 100-byte file header on page 0, SQLite-identical varint encoding
- **B+tree**: clustered table B+tree (key = rowid) and index B+tree sorted by (key, rowid) with an order-preserving key encoding — O(log N) index seeks, prefix lookups for composite indexes, and range scans
- **Overflow chains**: rows larger than a page spill the payload tail to a linked chain of overflow pages (SQLite's overflow-cell layout: local prefix + first chain page) — megabyte BLOBs/TEXTs round-trip exactly, and `SELECT` streams them without buffering the chain
- **Index range scans**: `WHERE indexed_col > ?` / `BETWEEN` plans an `IndexRange` (index seek + fetch only matching rows)
- **Append-mode splits**: right-edge inserts keep the old leaf 100% full (SQLite's `balance_quick` behavior) — sequential loads fill pages ~2x denser than naive mid-splits
- **Page recycling**: DELETE unlinks empty leaves onto the pager freelist; new allocations reuse freelist pages before growing the file; VACUUM reclaims the file tail at page granularity
- **Pager**: LRU page cache with bucket-keyed leaf fast path, freelist, page allocation, dirty page tracking, WAL-aware reads
- **WAL**: write-ahead log with CRC32 checksums, salt-based recovery, frame-level integrity — torn writes truncate recovery at the first bad frame
- **MVCC**: snapshot isolation via WAL frame indexing (page-level version tracking)
- **Row codec v2** (`RSQLDB02`): size-classed integers (1–9 bytes), LEB128 text/blob lengths, and rowid-alias elision — `id INTEGER PRIMARY KEY` is stored as a 1-byte marker materialized from the B+tree key, like SQLite's record format
- **Committed-view read concurrency**: lock-free per-thread memo of the committed WAL view — readers resolve the latest committed page versions without touching shared atomics on the hot path
- **Page spill for big write transactions**: DELETE-journal-mode transactions spill dirty pages instead of holding them all in memory — bounded RSS for million-row write txns

### Tooling

- **CLI shell**: `rustqlite-cli` with table/JSON/CSV/line output modes and dot commands
- **HTTP/JSON server**: `rustqlite-server` with `/query`, `/execute`, `/health` endpoints
- **Benchmarks**: criterion harnesses against rusqlite (SQLite) for point lookups, range scans, inserts, joins, concurrency; `bench_compare` head-to-head with answer-equality asserts; sqlx-native vs sqlx-sqlite comparison

### Plugin system (SQLite-style extensions)

- **User functions**: scalar (`create_function`) and aggregate (`create_aggregate`) functions in safe Rust — planner-visible, GROUP BY-integrated, arity-checked, built-ins protected from shadowing
- **Collations**: `NOCASE` / `RTRIM` built-ins plus user-defined sequences (`create_collation`), honored by `ORDER BY … COLLATE` and comparison operators
- **Virtual tables**: `CREATE VIRTUAL TABLE … USING module(...)` with SQLite's full callback protocol — `xCreate`/`xConnect`/`xBestIndex` constraint pushdown, cursors, and writable modules (`xUpdate`) — persisting across reopen like SQLite's runtime modules
- **Page codecs**: pluggable page encode/decode (`PRAGMA codec`), the SEE/ZIPVFS-style hook — XOR codec included as a working example with file markers and safe-refuse-on-wrong-codec
- **Dynamic extensions in any language**: compile against `include/rustqlite_ext.h`, export `rustqlite_extension_init`, load with `Database::load_extension` — working examples in **C, C++, Zig, and Rust** (`plugins/`)
- **SQLite-shaped C ABI**: the `rustqlite_*` family (`open`/`exec`/`prepare_v2`/`step`/`bind`/`column`/`load_extension`, …) mirroring `sqlite3_*` argument order and semantics — the binding layer for drop-in compatibility
- **Prepared statements** (`Database::prepare` + `Statement::bind/step/reset`): parsed and planned once, rebindable, and **streaming** — scans/ranges/filters/projections/limits (and vtab scans) deliver rows in batches of 64 without materializing the result set, with early termination

### sqlx: native Rust driver (`features = ["sqlx"]`)

**sqlx 0.9 works with rustqlite as a plain library dependency** — no
`libsqlite3.so`, no C ABI, no `[patch.crates-io]`, no C toolchain. The
`sqlx_driver` module implements sqlx-core's `Database` traits directly
against the engine, so all of sqlx's generic machinery — `Pool`,
`query()` / `query_as()` / `query_scalar()`, `FromRow` derive,
transactions with isolation levels, `fetch` streaming, statement
logging, pool timeouts — works out of the box:

```rust
use rustqlite::sqlx_driver::{RustqlitePool, RustqliteConnectOptions};

let opts = RustqliteConnectOptions::filename("app.db").create_if_missing(true);
let pool = RustqlitePool::connect_with(opts).await?;

sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
    .execute(&pool).await?;
let id: i64 = sqlx::query_scalar("INSERT INTO users (name) VALUES (?) RETURNING id")
    .bind("Ada").fetch_one(&pool).await?;
```

- **100% safe Rust** — no FFI, no lifetime-erased handles, trivially cross-compilable
- **Faster than sqlx-sqlite**: executes inline in the async task instead of ferrying every command and row across a dedicated worker thread + FFI — see [sqlx driver vs sqlx-sqlite](#sqlx-driver-vs-sqlx-sqlite)
- **Full sqlx type surface**: `chrono` (`DateTime`, `NaiveDate/Time`, `time` delta), `uuid`, `Json<T>`, `bool`, `&str`/`String`, `f32`/`f64`, blobs — encode/decode parity with sqlx-sqlite
- **SQLite snapshot isolation between connections**: readers never see uncommitted writes (they wait, up to the busy timeout, then get `SQLITE_BUSY` — exactly like SQLite); read-only transactions never block readers; a dropped connection rolls back whatever transaction it left open, so one connection can never wedge the pool
- **URL scheme**: `rustqlite://app.db`, `rustqlite://:memory:?cache=shared`, `mode=rwc` / `immutable` options — drop-in-shaped for sqlx-style config

### sqlx & sea-orm compatibility (drop-in `libsqlite3`)

- **`compat/`** exports the real `sqlite3_*` C ABI (124 symbols) on the engine and ships a drop-in `libsqlite3-sys` replacement, so **unmodified crates.io sqlx 0.9 and sea-orm 2.0 run on rustqlite** via one `[patch.crates-io]` line
- SQLite-exact error messages + extended result codes (`SQLITE_CONSTRAINT_UNIQUE`, `SQLITE_MISMATCH`, …) — what sqlx's `error_kind()` and sea-orm's `DbErr` classify on
- Full UPDATE constraint semantics: sequential unique-index checking (ratchets pass, swaps conflict), `OR IGNORE`/`OR REPLACE`, rowid moves (`UPDATE t SET id = X`), collation-aware (NOCASE) unique probes, atomic statement aborts
- `sqlx-interop/` — the integration workspace that proves it: a checked-in sea-orm 2.0 + sqlx 0.9 app compiled and passing tests against `compat/`'s `libsqlite3.so` replacement

## Architecture

### Overview

rustqlite is a layered embedded SQL engine. The architecture mirrors SQLite's (page format, B+tree, WAL, SQL front-end / back-end split) but is implemented from scratch with clean separation of concerns and modern Rust idioms. Each layer has a single responsibility and a well-defined contract with the layer above; dependencies flow strictly downward — the executor never touches the pager directly, it goes through the B+tree.

```
┌──────────────────────────────────────────────────────┐
│  Public API                                           │
│  Database, Connection, Params, Statement, sqlx driver│
├──────────────────────────────────────────────────────┤
│  Executor                                             │
│  Plan → rows (streaming drivers + collect-all)        │
│  Operators: Scan, Filter, Project, Sort, Limit,      │
│    Aggregate, Window, Join, Distinct, Union,          │
│    Intersect, Except, RowidLookup, RowidRange,        │
│    IndexRange, IndexNestedLoopJoin, Insert, Update,   │
│    Delete · parallel worker split (executor::parallel)│
├──────────────────────────────────────────────────────┤
│  Planner                                              │
│  AST → logical Plan                                   │
│  Name resolution, aggregate rewriting, index hints,  │
│    rowid rewriting, fused-scan planning               │
├──────────────────────────────────────────────────────┤
│  Schema                                               │
│  Catalog: tables, indexes, views, triggers            │
│  Stored as rows in the schema table (page 0)          │
├──────────────────────────────────────────────────────┤
│  SQL                                                  │
│  Lexer (tokenizer) → Parser (recursive descent) →     │
│    AST                                                │
├──────────────────────────────────────────────────────┤
│  Storage                                              │
│  Pager (LRU cache, freelist, file I/O, codec hooks)  │
│    → B+tree (table + index, leaf + interior)         │
│    → WAL (CRC32, salt, recovery) + committed view    │
│    → MVCC (snapshot isolation via WAL frames)         │
│    → Row codec v2 (compact binary)                    │
└──────────────────────────────────────────────────────┘
```

The plugin system adds one horizontal layer touching all five (C ABI + extension ABI → plugin registry → SQL parse / planner / executor hooks → catalog vtab instances + pager codec hooks); see [Plugin system design](#plugin-system-design) below.

### Storage layer

**Page format.** The database file is a sequence of fixed-size pages (default 4 KiB). Page 0 begins with a 100-byte file header (magic `RSQLDB01`, page size, change counter, page count, freelist head + count, schema cookie + format version, cache hint, largest root page, text encoding, user version, application ID) followed by the B+tree header for the schema table. B+tree page headers are 12 bytes (type / cell count / cell-content start / right-most child), followed by the cell pointer array and the cell content area growing downward. Page types: `0x0D` leaf table, `0x05` interior table, `0x0A` leaf index, `0x02` interior index.

**Varints.** Big-endian, 1–9 bytes, 7 bits per byte with the high bit as continuation, the 9th byte using all 8 bits — 64 bits total, exactly SQLite's format, so a rustqlite database could in principle be read by SQLite and vice versa given compatible payload encodings.

**Pager.** Owns the file handle, an LRU page cache, the freelist, the schema cookie, and the page count. `get_page` (cache → disk miss → insert with LRU eviction), `allocate_page` (freelist pop, else file extend), `free_page` (freelist push), `flush` (write dirty pages, update header, fsync). Pages are shared handles so the B+tree can hold multiple references during splits; per-page locks give readers true parallelism. Codec hooks wrap the two I/O choke points (main-file read, DELETE-mode flush); page 0 keeps its first 100 bytes plain with a `RQLCODEC:<name>` marker at bytes 72..100.

**B+tree.** Table B+tree: leaf cells are `(varint rowid, varint payload_len, payload)`, interior cells `(u32 left_child, varint key)`; point lookup descends from root, insert redistributes on overflow and propagates splits up (root split creates a new root; right-edge inserts take SQLite's `balance_quick` append-mode split keeping the old leaf full), delete unlinks without rebalancing (like SQLite). Index B+tree: leaf cells `(varint rowid, key)`, interior cells `(u32 left_child, varint rowid, key)` with order-preserving composite keys. `max_rowid_hint` is an O(log N) rightmost descent used by the parallel executor for range splitting.

**WAL.** A separate `<db>-wal` file of committed-but-not-checkpointed page writes. Header (32 B: magic, format version, page size, checkpoint sequence, salts, checksums) + frames (24 B header: page ID, commit marker, copied salts, running checksum). The checksum is a running CRC32 over (page_id + commit marker + page data); a torn write at frame N invalidates everything after it — recovery stops at the first mismatch. Checkpoint applies valid frames to the main file and resets the log.

**MVCC.** Snapshot isolation at page level: each transaction gets a monotonic TXID; the WAL is the source of truth for committed writes; a snapshot at TXID t sees the latest version of each page from frames committed at TXID ≤ t (a `VersionTracker` binary-searches each page's frame list). On top of this, a lock-free per-thread committed-view memo (and BEGIN-time root snapshots for point lookups under open write transactions) lets readers resolve the committed view without shared-atomics contention.

**Row codec v2.** Rows are concatenated `Value::encode` outputs — 1-byte tag (NULL/INTEGER/REAL/TEXT/BLOB) + size-classed payload: integers 1–9 bytes by magnitude, LEB128 lengths for text/blob, rowid-alias elision for `INTEGER PRIMARY KEY`. Decoding pads missing columns with NULL (handles `ALTER TABLE ADD COLUMN` back-fill), and selective decode reads only the columns a predicate or projection needs instead of the whole row.

### SQL layer

**Lexer.** Hand-written state machine with line/column tracking: case-insensitive keywords, identifiers, double-quoted identifiers, decimal/hex integer literals, float literals with exponent, single-quoted strings with `''` escape, blob literals `x'...'`, parameters (`?`, `?N`, `:name`, `@name`, `$name`), multi-char-first operators (`<=`, `>=`, `!=`, `<>`, `==`, `||`, `<<`, `>>`), `--` and `/* */` comments.

**Parser.** Recursive descent with precedence climbing (OR → AND → comparisons → `|` → `^` → `&` → shifts → `+`/`-`/`||` → `*`/`/`/`%` → unary). Statements: `CREATE {TABLE|INDEX|VIEW|TRIGGER}`, `DROP`, `INSERT` (all conflict clauses, `VALUES`/`SELECT`/`DEFAULT VALUES`, UPSERT, RETURNING), `SELECT` (WITH, DISTINCT, joins, WHERE, GROUP BY, HAVING, WINDOW, ORDER BY, LIMIT/OFFSET, set ops), `UPDATE` (SET, FROM, WHERE, RETURNING), `DELETE` (WHERE, RETURNING, ORDER BY, LIMIT), `BEGIN`/`COMMIT`/`ROLLBACK`, `SAVEPOINT` family, `PRAGMA`, `ATTACH`/`DETACH`, `VACUUM`, `EXPLAIN`. Postfix: `COLLATE`, `IS [NOT] NULL`, `[NOT] LIKE/GLOB/REGEXP/MATCH`, `[NOT] IN`, `[NOT] BETWEEN`, `FILTER`, `OVER`. The AST uses `Box` for recursive types; it is a faithful representation of the source SQL with minimal desugaring.

### Schema layer

The catalog is an in-memory map of name → `Arc<Table>` / `Arc<Index>` / `Arc<View>` / `Arc<Trigger>`, persisted as rows in the schema table (rooted at page 0) with SQLite's `(type, name, tbl_name, rootpage, sql)` columns. `Database::open` scans the schema table and re-parses each row to reconstruct the catalog; DDL bumps the schema cookie and updates the table. `Table` carries column definitions (affinity, constraints, defaults, generated columns), the root page, the `rowid_alias` column index, `without_rowid`/`strict` flags, and the optional `vtab` instance.

### Planner

Converts an `ast::SelectStatement` into a `Plan` tree: name resolution through a scope stack, plan shape `Scan → Filter (WHERE) → Aggregate (GROUP BY + aggs) → Filter (HAVING) → Window → Distinct → Project → Sort → Limit`, and aggregate rewriting (each `SUM(x)` in the projection becomes a column reference to the `Aggregate` operator's pre-computed output). Beyond the basics it plans: `RowidLookup` for `WHERE id = ?`, `RowidRange` for `BETWEEN`/comparison rowid predicates, `IndexRange` + `IndexNestedLoopJoin` for indexed predicates, fused scan shapes (Filter-over-Scan over selective decode), rowid-inside-expressions rewriting with a hidden rowid slot, and rowid-via-index-range plans. Not yet: predicate pushdown into all scan shapes, join reordering, subquery decorrelation, cost-based index choice.

### Executor

Walks a `Plan` tree and produces rows. OLTP plan shapes (Scan with rowid-resume, RowidRange, Filter, Project, Limit, vtab cursors) run as **resumable streaming drivers** — rows arrive in batches, `LIMIT k` stops the walk at the k-th match, non-matching rows decode only the predicate's columns (selective decode). Everything else executes once into materialized rows (still a single parse + plan per prepare). The expression evaluator walks `Expr` against the current row, qualified join columns, and bound parameters.

**Intra-statement parallelism** (`src/executor/parallel.rs`, 2026-09) is the tier SQLite's single-threaded executor cannot follow: large single-table aggregates (COUNT/SUM/AVG/MIN/MAX, optional simple filters; GROUP BY bare/compiled), and bounded top-N (`ORDER BY ... LIMIT k [OFFSET n]`) split the table's rowid space across `std::thread::scope` workers:

- The rowid range tiles into inclusive ranges — worker 0's range extends to `i64::MIN` so explicit rowid-0/negative rows are never dropped. Each worker builds its own `Btree` handle over the shared `&Pager` and runs the serial path's own `FusedWalk` / `HashGrouper` / keep-heap machinery over its `[lo, hi]` range, so per-row semantics are identical by construction.
- Aggregates merge partial accumulators in **range order** with **op-aware layouts** (SUM `(i_sum, sum, is_int)`, AVG `(count, sum)`, COUNT `count`, MIN/MAX min/max — the merge is not a byte-blit); GROUP BY first-seen group order is reproduced exactly; DISTINCT replays via set-union.
- Top-N workers keep per-range bounded heaps under the same strict total order (equal keys break on rowid, ASC and DESC) and merge under that order — bit-identical to the serial streaming top-N, including OFFSET windows.
- Safety comes from the engine's aliasing model — a statement executes under `&Database` while writers need `&mut Database`, so no writer can interleave within one call — plus per-page locks. Workers decline while any transaction is open (they are foreign to the committed-view TLS) and any worker bail aborts the attempt and falls back to the serial path — answers can never diverge.
- `PRAGMA parallel_scan` gates it (default ON, min-rows 131072; 0/OFF is bit-identical serial). The threshold sits above every existing test/bench table, so sub-threshold paths are unchanged.

### Public API

```rust
pub struct Database { /* pager + catalog + plugins + committed view */ }

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database>;
    pub fn open_in_memory() -> Result<Database>;
    pub fn execute<P: Params>(&mut self, sql: &str, params: P) -> Result<()>;
    pub fn query<P: Params>(&self, sql: &str, params: P) -> Result<Vec<Row>>;
    pub fn query_with_columns<P: Params>(&self, sql: &str, params: P) -> Result<(Vec<String>, Vec<Row>)>;
    pub fn prepare(&self, sql: &str) -> Result<Statement>; // streaming, rebindable
    // + create_function / create_aggregate / create_collation / create_module
    // + load_extension, set_busy_timeout, pragma accessors
}
```

The API is rusqlite-shaped: `execute` for statements without rows, `query` for those with. `query` takes `&self` — concurrent readers can share one `Database` and query simultaneously; writers need `&mut Database`, which the type system turns into the concurrency contract.

### Plugin system design

```
┌────────────────────────────────────────────────────────────┐
│  C ABI (ffi.rs)         rustqlite_* family + rql_api table │
│  Extension ABI (plugin/abi.rs)  C/C++/Zig/Rust .so loading │
├────────────────────────────────────────────────────────────┤
│  Plugin registry (plugin/mod.rs)                           │
│    scalars · aggregates · collations · vtab modules ·      │
│    page codecs  — Arc COW maps, thread-local scope         │
├──────────────┬──────────────────┬──────────────────────────┤
│ SQL: CREATE  │ Planner: plugin  │ Executor:                │
│ VIRTUAL      │ aggregates via   │ - call_scalar fallback   │
│ TABLE parse  │ is_aggregate_call│ - exec_plugin_aggregate  │
│              │                  │ - vtab_exec drivers      │
├──────────────┴──────────────────┴──────────────────────────┤
│ Storage: Table::vtab (catalog) + Pager codec hooks         │
└────────────────────────────────────────────────────────────┘
```

**Dispatch model.** The registry is a `RwLock<Arc<PluginRegistry>>` on `Database`. Every statement installs a snapshot into a thread-local scope (`PluginScopeGuard`, the same lifetime pattern as the correlated subquery bridge), so evaluator code deep in the tree resolves plugins without parameter threading. An `AtomicBool has_plugins` fast path keeps zero-plugin databases at one relaxed atomic load per statement — plugin-less throughput is unchanged. **Virtual tables** attach a `VtabInstance` to the catalog `Table`; scans route through `executor::vtab_exec`, WHERE conjuncts of the shape `vtab_col <op> const/param` are offered to `best_index`, marked-handled constraints become bound `xFilter` arguments, the rest stay engine-applied residuals; DML maps to `xUpdate` ops (SQLite argv protocol); `CREATE VIRTUAL TABLE` persists a schema row and reconnects when the module is registered, matching SQLite's runtime-module linkage. Statement caching is invalidated on plugin registration (plans may hold vtab-affected `Arc<Table>`).

### Design trade-offs

**What we did well**

- **Layered architecture** — each layer has a clean contract; you can swap the executor without touching storage.
- **Type safety** — Rust's ownership model turns the concurrency contract into compile-time facts (`&self` reads parallelize, `&mut self` writes serialize); the `Error` enum centralizes failure handling.
- **B+tree correctness** — randomized insert/lookup/scan/delete testing; the split logic is the trickiest part and is well-tested.
- **WAL integrity** — CRC32 + salt + running checksum makes torn-write recovery robust; verified by crash tests that abort at every statement boundary.
- **Concurrency model** — `&self` reads + per-page locks + scoped worker threads give three distinct concurrency tiers with zero unsafe code in the engine.
- **Parallel determinism** — workers run the serial path's own machinery and merge in range order, so answers are identical by construction (the one documented latitude: the last ULP of a REAL SUM, below).

**What we cut (deliberate omissions)**

- **Multi-writer transactions**: one writer at a time, `SQLITE_BUSY` + busy timeout — SQLite's own durability contract. True concurrent writers would need a page-level conflict tracker; documented rather than diverged from.
- **The VDBE**: SQLite's bytecode executor buys statement portability at dispatch cost; rustqlite's direct Rust call tree is a large part of why serial scans run 2–5x faster.
- **Async runtime inside the engine**: the engine stays synchronous and `Send`+`Sync`; async lives in the sqlx driver layer, which runs statements inline in the task (no dedicated worker thread).

### Source layout

```
src/
├── lib.rs              # Crate root, re-exports, sqlx_driver feature gate
├── error.rs            # Error enum + Result alias
├── api.rs              # Database, Params, execute/query
├── statement.rs        # Streaming prepared statements
├── ffi.rs              # SQLite-shaped C ABI (rustqlite_* family)
├── types/              # Value, Affinity, Row
├── storage/
│   ├── page.rs         # Page, PageType, FileHeader
│   ├── pager.rs        # Pager (LRU cache, freelist, codec hooks)
│   ├── btree.rs        # Btree, Cell, varint
│   ├── wal.rs          # Wal, headers, recovery
│   ├── mvcc.rs         # Snapshot, VersionTracker, committed view
│   ├── row_codec.rs    # encode_row / decode_row (+ selective decode)
│   └── integrity.rs    # PRAGMA integrity_check walker
├── sql/                # Lexer, AST, Parser
├── schema/             # Catalog: tables, indexes, views, triggers
├── planner/            # Plan construction, aggregate rewriting
├── executor/
│   ├── mod.rs          # Operators, streaming drivers, fused scans
│   ├── expr.rs         # Expression evaluation, scalar functions
│   ├── parallel.rs     # Intra-statement worker split (aggregates, top-N)
│   └── vtab_exec.rs    # Virtual-table scan/update drivers
└── plugin/             # Registry, ABIs, dynamic extension loading
compat/                 # Drop-in libsqlite3 C ABI + libsqlite3-sys replacement
sqlx-interop/           # sea-orm 2.0 + sqlx 0.9 integration testbed
plugins/                # Extension examples in C, C++, Zig, Rust
benches/                # criterion harnesses vs rusqlite
examples/               # 145 probes/benchmarks (bench_compare, probe_*, ...)
tests/                  # The full test matrix (see Testing)
include/rustqlite_ext.h # Extension header for C/C++/Zig
```

## Performance vs SQLite

Head-to-head against rusqlite with bundled SQLite on identical workloads, `cargo run --release --example bench_compare` — **every timing row carries an answer-equality assert against SQLite before it is timed**. Measured 2026-09-07 (2 vCPU, best-of-5, after the intra-statement parallel aggregation, parallel top-N, and bounded-memory passes). Ratio > 1 = rustqlite faster.

### Serial workloads

| Workload                              | rustqlite   | SQLite     | Ratio            |
|---------------------------------------|-------------|------------|------------------|
| Single-row inserts (auto-commit, 1k)  | **804 µs**  | 1.75 ms    | **2.18x faster** |
| INSERT in txn (10k rows)              | **7.19 ms** | 12.78 ms   | **1.78x faster** |
| INSERT in txn (100k rows)             | **74.3 ms** | 134.6 ms   | **1.81x faster** |
| Multi-row VALUES (10k rows)           | **4.43 ms** | 6.18 ms    | **1.40x faster** |
| Point lookup by rowid (1k ops)        | **218 µs**  | 446 µs     | **2.05x faster** |
| Range scan (10 rows)                  | **977 ns**  | 1.98 µs    | **2.03x faster** |
| Range scan (100 rows)                 | **9.0 µs**  | 15.9 µs    | **1.76x faster** |
| Range scan (1000 rows)                | **67.7 µs** | 156.6 µs   | **2.31x faster** |
| Range scan (5000 rows)                | **343 µs**  | 786.5 µs   | **2.29x faster** |
| Full scan + COUNT with filter         | **174 µs**  | 455 µs     | **2.62x faster** |
| COUNT(*) bare (memoized)              | **92 ns**   | 2.5 µs     | **27x faster**   |
| Aggregate (SUM/AVG/MIN/MAX)           | **286 µs**  | 1.18 ms    | **4.13x faster** |
| GROUP BY (100 buckets)                | **684 µs**  | 1.83 ms    | **2.68x faster** |
| Indexed point lookup (1k ops)         | **350 µs**  | 608 µs     | **1.74x faster** |
| 2-table join (PK filter)              | 2.98 µs     | 2.94 µs    | parity (± noise) |
| 3-table join (PK filter, 50 out)      | **16.0 µs** | 21.5 µs    | **1.35x faster** |
| 2-table join + GROUP BY               | **1.73 ms** | 2.90 ms    | **1.68x faster** |
| UPDATE by PK (1k ops)                 | **1.70 ms** | 1.85 ms    | **1.09x faster** |
| UPDATE range (val > 5000)             | **670 µs**  | 1.13 ms    | **1.69x faster** |
| DELETE by PK (1k ops)                 | **636 µs**  | 1.37 ms    | **2.15x faster** |
| Mixed 80/20 read/write (5k ops)       | **1.87 ms** | 2.45 ms    | **1.31x faster** |

### Large-table parallel workloads (1M rows, 2 workers)

rustqlite splits the scan across worker threads; SQLite's executor is single-threaded by design — one query, one core, forever.

| Workload (1M rows)                    | rustqlite   | SQLite     | Ratio            |
|---------------------------------------|-------------|------------|------------------|
| Big aggregate (COUNT/SUM/AVG/MIN/MAX) | **17.2 ms** | 132.4 ms   | **7.7x faster**  |
| Filtered aggregate (val > 500000)     | **13.2 ms** | 74.4 ms    | **5.7x faster**  |
| GROUP BY (10k buckets + SUM)          | **47.5 ms** | 282.5 ms   | **5.9x faster**  |
| Top-25 ORDER BY REAL DESC (full rows) | **73.6 ms** | 214.8 ms   | **2.9x faster**  |
| Top-50 ORDER BY INT DESC OFFSET 100   | **29.0 ms** | 68.5 ms    | **2.4x faster**  |

### Where the wins come from

- **OLTP inserts**: a byte-level fast-path scanner executes single-row literal `INSERT ... VALUES (...)` without building tokens, an AST, or a plan — and `:memory:` databases skip per-statement file writes entirely (lazy write-back). Even with unique SQL text per statement (the worst case for caching), we beat SQLite's re-prepare cost.
- **Analytical scans / aggregates**: fused scan drivers with selective column decode beat SQLite 2–5x serially on scans, filtered COUNTs, and aggregates — before the parallel split compounds it. COUNT(*) is memoized on the B+tree (27x).
- **Point lookups**: the bucket-keyed 2-way leaf cache + cursor-ix cell bias makes rowid probes ~2x.
- **Rowid range scans**: `WHERE id BETWEEN ? AND ?` runs a dedicated fused range-probe path — no pipeline setup, binary-searched leaf descent, early stop at the first cell past the range end.
- **UPDATE range / index scans**: the `IndexRange` plan node seeks the index and touches only matching rows; UPDATE rewrites payloads in place where possible (payload-patch fast path).
- **Top-N**: per-worker bounded keep-heaps under the statement's exact total order, merged range-ordered — the serial streaming top-N's own algorithm, split across workers.
- **Bulk inserts**: BTREE_APPEND rightmost descent + append-mode splits + codec v2 keep sequential loads dense.
- **Join point filters**: IndexNestedLoopJoin + a warm statement cache beat SQLite's prepared-statement path on point-filtered joins; the unfiltered 2-table PK join sits at parity.
- **Concurrency**: see the [concurrency section](#concurrency-vs-sqlite) — the 8.3x concurrent-read and 5.7–8.3x parallel-aggregate multipliers sit on top of these serial wins.

### Trajectory

The 2026-08-28 baseline had rustqlite 1.1–8.7x *slower* than SQLite on these same rows. The 2026-09 sprints (fused scans, codec v2, append splits, payload-patch UPDATE, leaf cache, mimalloc wake tuning, committed-view concurrency, parallel executor) flipped every row to a win; the full ledger of each sprint lives in git history (`git log --oneline`).

Run the benchmarks yourself:

```bash
cargo run --release --example bench_compare             # full head-to-head
cargo run --release --example bench_sqlx_native --features sqlx
cargo bench --bench sqlite_comparison                  # criterion matrix
cargo bench --bench point_lookup -- --quick
cargo bench --bench range_scan  -- --quick
cargo bench --bench insert      -- --quick
cargo bench --bench join        -- --quick
```

## Resource consumption vs SQLite

| Metric                                        | rustqlite    | SQLite     | Verdict                                    |
|-----------------------------------------------|--------------|------------|--------------------------------------------|
| DB file size (10k rows, on disk)              | **262.14 KB**| 262.14 KB  | **byte-exact** (4 KiB pages, codec v2)     |
| Peak RSS (100k insert+count, incl. 1M-row parallel section) | **26.6 MB** | 29.3 MB | **0.91x — lower**                          |
| Stripped binary (CLI)                         | 3.10 MB      | ~2.06 MB   | 1.5x larger — deliberate (see below)       |
| WAL commit latency                            | **25.3 µs/txn** | 28.5 µs/txn | **1.13x faster** (delete journal: 6.2x)  |

Memory stays at parity or better on every measured shape: the streaming fused
scan drivers decode rows in batches instead of materializing result sets,
`LIMIT k` stops the walk at the k-th match, big write transactions spill
pages mid-transaction in both journal modes (bounded RSS), GROUP BY output
streams from the owned grouper in serving-sized batches instead of
materializing the whole result set, WAL checkpoints copy pages through a
bounded 1 MiB run buffer (a 7250-page first-build commit previously
materialized the whole 29 MiB WAL), and the parallel workers' per-range
scratch is bounded (one FusedWalk/grouper + a keep-sized heap each). The
single resource regression is deliberate: ~140 KiB of mimalloc in exchange
for 1.5–2.1x write throughput (opt-out: `default-features = false`); the
rest of the binary delta is the feature surface (plugins, sqlx driver,
parallel executor) that SQLite ships as separate extensions.

### Production torture matrix (memory columns, 2026-09-08 pass)

The 18-section torture matrix (`cargo run --release --example
prod_torture`, one isolated child process per engine per section, peak
RSS measured via VmHWM / mach / Win32, best-of-2 per section). The
2026-09-08 pass adds the process-wide THP disable (see below) — on
`THP=always` kernels it cut 30-50% off every memory column while every
time column stayed a win:

| Section (scale 1.0)                 | rustqlite | SQLite | Mem verdict |
|-------------------------------------|-----------|--------|-------------|
| S01 bulk load 1M (txn)              | 36 MB     | 35 MB  | TIE (0.99x) |
| S02 full scan + aggregates 1M       | 36 MB     | 35 MB  | TIE (0.96x) |
| S04 top-N ORDER BY 1M               | 36 MB     | 35 MB  | TIE (0.97x) |
| S05 point lookups 20k @1M           | 36 MB     | 35 MB  | TIE (0.97x) |
| S06 range 100k rows materialized    | 36 MB     | 35 MB  | TIE (0.97x) |
| S08 wide rows 2KB × 25k             | 57 MB     | 57 MB  | TIE (1.02x) |
| S09 blobs 64KB × 1k                 | 72 MB     | 73 MB  | TIE (1.01x) |
| S15 rollback 500k txn               | 21 MB     | 20 MB  | TIE (0.97x) |
| S07 random inserts 300k             | 16 MB     | 14 MB  | 0.87x       |
| S10 LIKE scan 100k                  | 9 MB      | 8 MB   | 0.93x       |
| S11 IN-list 5000 literals           | 12 MB     | 10 MB  | 0.78x       |
| S12 5-index load                    | 19 MB     | 17 MB  | 0.87x       |
| S13 sustained 2M ops (leak probe)   | 7 MB      | 5 MB   | 0.73x (no leak: 6→7 MB flat) |
| S14 churn + reclaim (file)          | 13 MB     | 8 MB   | 0.62x       |
| S16 8r+2w concurrency               | 10 MB     | 8 MB   | 0.86x       |
| S17 open 1M-row file + SUM          | 14 MB     | 7 MB   | 0.50x       |
| S18 differential hash               | 9 MB      | 8 MB   | 0.93x       |
| S03 GROUP BY 100k buckets           | 57 MB     | 37 MB  | 0.65x — see the gap ledger |

**The 2026-09-08 THP discovery**: mimalloc's `allow_thp=0` prctl path
only runs at mimalloc init — which happens on the first Rust-runtime
allocation, before any engine code can set the option. On
`transparent_hugepage=always` kernels that meant every touched 4 KiB
allocation page inside mimalloc's arena reservations materialized a
2 MiB huge page: the same engine+workload measured 22 MB where the
identical binary on a `madvise` kernel measured 12 MB. The engine now
runs `prctl(PR_SET_THP_DISABLE)` from an `.init_array` constructor —
before the runtime's first allocation — with `RSQL_ALLOW_THP=1` as the
opt-out for huge-page-friendly throughput builds. Measured impact on a
`THP=always` kernel: S17 28→14 MB, S13 12→7 MB, S14 24→13 MB, S11
22→12 MB, S16 21→10 MB, and the micro-benchmark baseline 10.4→8.3 MB —
with every time column unchanged (S02 6.07x, S16 7.12x).

Earlier passes: the 2026-09-07 bounded-memory pass closed S17 72→14 MB,
S14 44→13 MB and S03 96→57 MB (bounded WAL checkpoint, 40 B aggregate
states, per-burst allocator drains). Every time column stayed a win or
tie throughout.

## Concurrency vs SQLite

| Concurrency workload                        | rustqlite vs SQLite | Why                                             |
|---------------------------------------------|--------------------|--------------------------------------------------|
| Concurrent reads (8 threads, criterion)     | **8.3x faster**    | per-page locks + interior-mutability pager vs SQLite's serialized connection mutex |
| 1 writer + 7 readers (sqlx pool)            | **2.0x faster**    | lock-free per-thread committed-view memo; readers never block on the writer |
| 8-task one-pool (sqlx, same API)            | **5.1x faster**    | inline async execution vs worker thread + FFI   |
| 8-conn concurrent reads (sqlx)              | **2.8x faster**    | MRMW shared pages                                |
| Mixed 80/20 (sqlx, high write fan-out)      | 1.05x              | writer gate + commit fsync dominates; reads stay 2.8x |
| Intra-statement parallel aggregates (1M)    | **5.9–8.3x faster**| worker-split rowid ranges + range-ordered merge — SQLite's executor is single-threaded **by design** |
| Intra-statement parallel top-N (1M)         | **2.4–2.9x faster**| per-worker keep-heaps under the statement's total order, merged range-ordered |
| Multi-connection writers                    | SQLite-equivalent | single-writer + BUSY + busy timeout, exactly SQLite's own contract |

SQLite executes one query on one core, forever; a connection serializes every statement
through its mutex. rustqlite has three concurrency tiers, each beyond that envelope:

1. **Inter-connection (MRMW reads)**: multiple readers run truly in parallel on shared
   pages — per-page locks + an interior-mutability pager. Readers see a consistent
   committed snapshot (WAL frame indexing + a lock-free per-thread committed-view memo);
   no dirty reads (`examples/probe_dirty_read.rs`). 8-thread concurrent reads are **8.3x**
   SQLite's throughput.
2. **Inter-statement (sqlx layer)**: the async driver executes statements inline in the
   task — no dedicated worker thread, no FFI ferry — 5.1x on 8-task pools, 2.0x on
   1-writer/7-reader, with snapshot isolation between connections (readers wait for the
   busy timeout then `SQLITE_BUSY`, exactly like SQLite).
3. **Intra-statement (parallel executor)**: large single-table aggregates, GROUP BYs, and
   top-N sorts split across worker threads with deterministic range-ordered merges —
   5.9–8.3x on 1M-row aggregates, 2.2–2.4x on top-N. `PRAGMA parallel_scan` gates it
   (default ON above 131072 estimated rows; 0/OFF is bit-identical serial). Workers
   decline while any transaction is open, and any worker bail falls back to the serial
   path — answers can never diverge.

## sqlx driver vs sqlx-sqlite (same sqlx 0.9 API)

`cargo run --release --example bench_sqlx_native --features sqlx` — the native Rust driver
against sqlx-sqlite (C SQLite) through the identical sqlx API and pool options:

| Scenario                     | rustqlite | sqlx-sqlite | Speedup |
|------------------------------|-----------|-------------|---------|
| INSERT + 3 binds             | 68.5 ms   | 104.1 ms    | 1.52x   |
| PK point lookup              | 39.3 ms   | 105.3 ms    | 2.68x   |
| UPDATE by PK                 | 70.0 ms   | 105.7 ms    | 1.51x   |
| filtered scan fetch_all      | 27.1 ms   | 69.8 ms     | 2.58x   |
| GROUP BY fetch_all           | 33.0 ms   | 93.7 ms     | 2.84x   |
| txn: 100 inserts             | 6.5 ms    | 24.3 ms     | 3.72x   |
| stream full table            | 2.6 ms    | 47.0 ms     | **18.4x** |
| 8-task concurrent (1 pool)   | 14.2 ms   | 73.0 ms     | 5.13x   |
| 8-conn concurrent reads      | 13.1 ms   | 36.9 ms     | 2.83x   |
| 8-conn mixed R/W 80/20       | 642 ms    | 674 ms      | 1.05x   |
| 1 writer + 7 readers         | 136.5 ms  | 272.0 ms    | 1.99x   |

The mechanism: sqlx-sqlite ferries every command and every row across a dedicated worker
thread and an FFI boundary; the native driver executes inline in the async task against
the engine's `Send + Sync` core. The 18.4x stream number compounds that with the engine's
batch-at-a-time streaming drivers (rows are produced in batches of 64 instead of
per-row FFI callbacks).

## SQLite file-format interop

rustqlite reads and writes **real SQLite database files** (the `fileformat2`
on-disk container): files created by the `sqlite3` CLI, Python's `sqlite3`,
rusqlite or any other SQLite tooling open directly in rustqlite — and files
rustqlite creates open in all of them, passing `PRAGMA integrity_check`.

### How it works

The engine's own native container (`RSQLDB04`) stays the default for files
it creates — it carries the page-codec plugin markers, the spill-file
bounded-memory transactions and the checksummed WAL. SQLite files get a
complete second format stack:

- **Reader** (`src/storage/sqlitefmt/reader.rs`): parses the 100-byte
  header (any page size 512 B–64 KiB, reserved bytes honored), folds
  committed `-wal` sidecar frames into the read view, walks table b-trees
  (rowid + `WITHOUT ROWID`), reassembles overflow chains with fileformat2's
  exact local-length formulas, and decodes records (serial types 0–9,
  12+) into engine values.
- **Writer** (`src/storage/sqlitefmt/writer.rs`): bulk-builds dense
  b-trees bottom-up with SQLite's own separator invariants (rowid bounds
  copied up for table trees, real entries pushed up for index trees —
  verified empirically against 3.46/3.53 files), writes overflow chains
  with the exact `K = M + (P−M) % (U−4)` split, `sqlite_autoindex_*`
  rows with NULL sql, `sqlite_sequence`, views, triggers, and header
  counters. Commits are atomic: temp file + fsync + rename.

Opening a SQLite file decodes it into the engine's in-memory state (every
SQL feature — planner, parallel scans, the C ABI, the sqlx driver — works
unchanged at native speed), and each commit boundary rewrites the file in
SQLite's format, so the file stays a genuine SQLite database at all times.

### Usage

```rust
// Open a SQLite file (auto-detected by magic):
let mut db = Database::open("created_by_sqlite3.db")?;
let rows = db.query("SELECT * FROM t", ())?;
db.execute("INSERT INTO t VALUES (…)", ())?;        // file stays SQLite format

// Create a NEW file in SQLite's format (readable by the sqlite3 CLI):
let mut db = Database::open_sqlite_format("new.db")?;
db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE)", ())?;
```

```sh
# CLI: same two forms
rustqlite-cli data.db                  # opens SQLite or native files
rustqlite-cli --sqlite-format new.db   # creates a SQLite-format file
```

`VACUUM INTO 'out.db'` on any database writes `out.db` as a SQLite file
(the native→SQLite export path).

### Verified by tests

`tests/sqlite_interop.rs` (rusqlite = real bundled SQLite as the other
side) covers both directions: all value types, `i64::MIN`, overflow
payloads spanning many pages, multi-level trees (3k+ rows), `INTEGER
PRIMARY KEY` alias storage, `TEXT PRIMARY KEY` autoindexes, unique and
expression indexes, `AUTOINCREMENT` + `sqlite_sequence` high-water
preservation, `user_version` / `application_id`, views, triggers,
`WITHOUT ROWID` records, explicit transactions, ROLLBACK, dump-on-drop,
and a live-WAL-sidecar read. Every file the engine writes is re-opened by
SQLite and must pass `PRAGMA integrity_check`. A dedicated CI job
(`interop`) additionally exercises the real `sqlite3` CLI against the
`rustqlite` CLI on Linux and macOS.

### Interop limitations

- SQLite-format databases are single-connection (the whole image is
  loaded on open; the file is rewritten per commit — O(file) per commit
  boundary, not O(dirty pages)). Use the native format for hot
  write-heavy workloads; SQLite-format mode targets interchange.
- Index keys larger than one page (huge TEXT/`WITHOUT ROWID` PK values)
  cannot be loaded: the engine's native index b-tree has no overflow
  chains. Table data of any size round-trips fine.
- Non-BINARY/NOCASE collations in index definitions fall back to binary
  ordering in the written file.
- `auto_vacuum` pointer-map pages are read fine but not written (output
  is always fully dense); UTF-16-encoded files are rejected (the engine
  is UTF-8 throughout).

## Remaining gaps vs SQLite

The honest ledger. Every `bench_compare` row is a win or statistical parity; the
remaining deltas are elsewhere:

- **Binary size (+1.0 MB, 1.5x)**: mimalloc (~140 KiB) plus the engine's own feature
  surface (plugins, sqlx driver, parallel executor) — `default-features = false` drops
  mimalloc; SQLite's ~2 MB is a 25-year head start of hand-tuned code size. This is the
  only resource metric where rustqlite is behind, and it is deliberate.
- **Multi-writer transactions**: one writer at a time, `SQLITE_BUSY` + busy timeout —
  SQLite's own durability contract, kept by design rather than diverged from. (SQLite
  itself is single-writer; this is parity, not a gap — but it bounds write fan-out.)
- **8-conn mixed R/W 80/20 at 1.05x**: at high write fan-out the writer gate + commit
  fsync dominates; reads stay 2.8x throughout. SQLite's WAL has the same shape.
- **Narrow serial wins**: the unfiltered 2-table PK join sits at parity, UPDATE by PK at
  1.09x — both grow with table size and index selectivity.
- **Numeric precision**: `AVG` rounds to 10 decimals (SQLite rounds differently in a few
  edge cases); parallel REAL SUMs merge partial sums in range order — the last ULP can
  differ from a serial scan (float addition is not associative; every parallel SQL engine
  takes this latitude, and SQLite's own float-SUM order is unspecified).
- **Compat surface**: `sqlite3_serialize`/`deserialize`, preupdate hooks, and
  unlock-notify are stubs in `compat/`; `sqlite_master` DDL text and a few `PRAGMA`
  result shapes continue to be tightened via differential tests.
- **Parallel executor coverage**: parallel top-N shipped (2026-09-07); unbounded
  parallel ORDER BY (chunk sort + k-way range-ordered merge) and the compiled
  no-GROUP-BY aggregate path with parameter filters still run serial. Complex predicates
  with bound params take the generic (still fast, but not worker-split) path.
- **Planner**: predicate pushdown into all scan shapes, join reordering, subquery
  decorrelation, and cost-based index choice (table statistics) are not implemented —
  today's plans are rule-based; SQLite's cost model makes better choices on a few
  adversarial join orders.
- **Read-ahead / prefetch**: batched page I/O for file-backed large scans (the pager
  dedupes but does not prefetch; sequential read-ahead would cut syscall count).
- **Memory on the torture matrix's hardest shapes** (2026-09-07 pass closed the worst
  of these — S03 96→57 MB, S14 44→13 MB, S17 72→14 MB, and the bench peak-RSS row is
  now a 0.91x win; what remains, measured at scale 1.0):
  - **S17 open 1M-row file + SUM (0.51x)** and **S14 churn (0.62x)**: the file-mode
    build transaction peaks ~3 MB over SQLite (WAL-side bookkeeping plus allocator
    fragmentation of the insert churn), and the child's whole build phase defines the
    HWM. SQLite's pager is 25 years of bounded-everything tuning; each remaining MB
    needs profiling at the allocator level.
  - **S03 GROUP BY 100k buckets (0.65x)**: the parallel GROUP BY holds all 100k group
    states in RAM (worker partials merge into the final grouper, streamed output, 40 B
    states) where SQLite sorts-and-streams through a disk-backed ephemeral structure.
    That hash-hold is also what makes it **4.0x faster**; `PRAGMA parallel_scan=0`
    drops to ~0.8x memory at ~2.6x speed. Reaching SQLite's flat profile needs an
    ephemeral-B-tree spill (SQLite's own temp-store design) — the documented next unit.
  - **S07/S10/S12/S13/S16/S18 (~0.73–0.93x, 1–2 MB each)**: the mimalloc
    baseline — its 64 KiB-per-size-class page granularity commits ~1 MB at
    `Database::open` where glibc packs the same allocations into ~0.4 MB. That is
    the documented cost of the 20–40% small-allocation throughput mimalloc buys
    (SQLite is measured against glibc); `default-features = false` recovers the
    baseline. The 2026-09-08 THP constructor already removed a 2-4x amplification
    layer that sat on top of this on `THP=always` kernels.
  - **S11 IN-list 5000 literals (0.78x)**: ~0.5 KB per literal of parse
    machinery — tokens, the AST list, the plan's clone of it — retained by the
    statement cache. SQLite compiles literal IN lists straight into a VDBE
    transient table without retaining three expression trees; the fix is
    Arc-sharing the predicate between AST and plan.

## Usage

### CLI

```bash
cargo run --release --bin rustqlite-cli -- /path/to/db.sqlite
# Or in-memory:
cargo run --release --bin rustqlite-cli -- :memory:
```

```sql
rustqlite> CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
rustqlite> INSERT INTO t (name) VALUES ('alice'), ('bob');
rustqlite> SELECT * FROM t;
rustqlite> .mode json
rustqlite> SELECT * FROM t;
rustqlite> .quit
```

### HTTP server

```bash
cargo run --release --bin rustqlite-server -- --db /path/to/db.sqlite --port 8080
```

```bash
curl -X POST http://localhost:8080/execute \
  -d '{"sql":"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"}'

curl -X POST http://localhost:8080/execute \
  -d '{"sql":"INSERT INTO t (name) VALUES (\"alice\")"}'

curl -X POST http://localhost:8080/query \
  -d '{"sql":"SELECT * FROM t"}'
# {"columns":["id","name"],"rows":[[1,"alice"]]}
```

### Library

```rust
use rustqlite::{Database, Value};

let mut db = Database::open("/tmp/my.db")?;
db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])?;
db.execute("INSERT INTO t (x) VALUES (10)", [])?;

let rows = db.query("SELECT x FROM t WHERE id = ?", vec![Value::Integer(1)])?;
assert_eq!(rows[0][0], Value::Integer(10));
```

### sqlx (async)

```toml
# your app's Cargo.toml — the sqlx facade with any runtime
rustqlite = { version = "0.1", features = ["sqlx"] }
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio"] }
```

```rust,ignore
use rustqlite::sqlx_driver::{RustqlitePool, RustqliteConnectOptions};

let pool = RustqlitePool::connect("rustqlite://app.db?mode=rwc").await?;

sqlx::query("INSERT INTO t (x) VALUES (?)").bind(10).execute(&pool).await?;

let total: i64 = sqlx::query_scalar("SELECT SUM(x) FROM t").fetch_one(&pool).await?;
```

### Drop-in libsqlite3 replacement (sqlx / sea-orm without code changes)

```toml
# your app's Cargo.toml
[patch.crates-io]
libsqlite3-sys = { path = "path/to/rust-sql/compat/rustqlite-compat" }
```

Unmodified sqlx 0.9 and sea-orm 2.0 then run against rustqlite's `libsqlite3.so`
(124 `sqlite3_*` symbols, SQLite-exact error messages and extended result codes).
`sqlx-interop/` is the checked-in testbed that proves it.

### Plugins & streaming statements

```rust
use rustqlite::{Database, Value, StepResult};
use rustqlite::plugin::{ScalarFunction, FnCtx};

struct Doubler;
impl ScalarFunction for Doubler {
    fn name(&self) -> &str { "double" }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> rustqlite::Result<Value> {
        Ok(Value::Integer(args[0].as_integer() * 2))
    }
}

let mut db = Database::open_in_memory()?;
db.create_function(Doubler)?;
db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])?;
db.execute("INSERT INTO t (id) VALUES (1), (2), (3)", [])?;

// SQLite-style streaming prepared statement.
let mut stmt = db.prepare("SELECT double(id) FROM t WHERE id > ?")?;
stmt.bind(1, Value::Integer(1));
while stmt.step()? == StepResult::Row {
    println!("{}", stmt.column_int(0));   // 4, 6
}
```

Load a compiled extension (C/C++/Zig/Rust):

```rust
db.load_extension("plugins/c/rot13.so", None)?;
let rows = db.query("SELECT rot13('hello')", [])?;   // "uryyb"
```

## Examples

See [`examples/`](examples/) — 145 probes and benchmarks:

- `basic.rs`: create, insert, query, update, delete, aggregates, group by
- `transaction.rs`: atomic transfer between accounts
- `batch.rs`: bulk insert + aggregation benchmarks
- `bench_compare.rs`: the full head-to-head vs SQLite (answer-equality asserts)
- `bench_sqlx_native.rs`: sqlx driver vs sqlx-sqlite
- `probe_parallel_scan.rs`: parallel executor scaling probe
- `probe_1w7r.rs`: 1-writer/7-reader concurrency probe
- `probe_dirty_read.rs`: snapshot-isolation verification

```bash
cargo run --example basic
cargo run --example transaction
cargo run --example batch
```

## Testing

The test matrix is modeled on SQLite's own methodology
([sqlite.org/testing.html](https://www.sqlite.org/testing.html)); 560+ tests in the
default matrix, all passing, plus the sqlx feature suite:

| SQLite technique (testing.html §) | rustqlite harness | What it verifies |
|---|---|---|
| TCL assert-heavy tests (§2) | all of `tests/` | every feature exercised through the public API with exact-value asserts |
| Branch coverage (§2.1) | `cargo test` + clippy exhaustive-match lint | every `Plan`/`Expr` arm handled and driven |
| Regression tests (§2.2) | `tests/regression.rs` | one test per historically-found bug, pinned forever |
| Boundary values | `tests/boundary.rs` | i64 MIN/MAX rowids, extreme index keys, LIKE/GLOB edges, deep nesting |
| I/O error injection (§3.2) | `tests/io_fault.rs` | ENOSPC, truncation, deleted files, read-only dirs — graceful `Err` + intact `integrity_check` |
| Crash / power-loss (§3.3) | `tests/crash_recovery.rs` | child process `abort()`s at EVERY statement boundary; committed baseline survives, in-flight txn is all-or-nothing, both journal modes |
| OOM injection (§3.5) | `tests/oom_fault.rs` (`--features oom-injection`) | counting allocator fails at each of hundreds of fault points; baseline survives every one |
| Corruption fuzz (§4.4) | `tests/db_corrupt_fuzz.rs` | byte strikes, structural strikes, truncations, corrupt WALs — never a panic or hang |
| SQL fuzz (§4.1) | `tests/sql_fuzz.rs` | mutation + structured fuzz run against BOTH engines — results must match |
| Differential testing | `tests/differential.rs` | randomized workloads vs real SQLite; row sets identical |
| `integrity_check` | `tests/integrity_check.rs` | full structural walk: freelist chains, every b-tree, bidirectional index↔table cross-verification |
| Soak / long-run | `concurrency_stress.rs`, `RUSTQLITE_FUZZ_ITERS` | 32-thread mixed workloads; long fuzz iterations |
| Concurrency (§5) | `concurrent_throughput.rs`, `committed_view.rs`, `wal.rs` | no deadlocks, no lost writes, no torn reads, snapshot consistency |
| Intra-statement parallelism | `tests/parallel_scan.rs` | parallel-vs-serial result equality for every parallel shape (incl. GROUP BY row order, top-N order), 300k-row SQLite cross-check, transaction-decline + PRAGMA-gate contracts |
| SQL Logic Tests | `tests/slt_runner.rs` + `tests/slt/` | the SLT format SQLite's core team uses |

Every fuzzer is seeded (`RUSTQLITE_FUZZ_SEED`) so failures reproduce exactly.

```bash
cargo test                                              # the whole matrix
cargo test --features sqlx --test sqlx_driver           # native sqlx driver
cargo test --test parallel_scan                         # parallel-vs-serial equality
cargo test --test crash_recovery                        # crash simulation
cargo test --features oom-injection --test oom_fault    # OOM injection
cargo clippy --all-targets --features sqlx              # 0 warnings (all configs)
cargo run --release --example bench_compare             # vs SQLite
```

## License

MIT OR Apache-2.0.

## Acknowledgments

The architecture is heavily inspired by SQLite (page format, B+tree layout, WAL design, testing methodology) and PostgreSQL (MVCC concepts, planner structure). The [SQLite Database File Format](https://www.sqlite.org/fileformat.html) document was an invaluable reference.
