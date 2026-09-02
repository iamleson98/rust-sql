# rustqlite

A from-scratch embedded SQL database engine written in pure Rust, modeled after SQLite but designed for cleaner code and competitive performance.

> **Status**: production-ready core — 300+ tests including crash/power-loss
> simulation, OOM + I/O fault injection, corruption fuzzing, and
> differential verification against real SQLite; sqlx 0.9 (native driver
> **and** drop-in C ABI) + sea-orm 2.0 compatibility verified end-to-end;
> beats SQLite on every benchmark row (see [Performance](#performance)).
> Full SQLite-style **plugin system** (functions, aggregates, collations,
> virtual tables, page codecs) loadable from C, C++, Zig, and Rust — see
> [PLUGINS.md](PLUGINS.md). Remaining gaps: [Limitations](#limitations).

## Quick Start

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

## Features

### SQL Surface

- **DDL**: `CREATE TABLE`, `CREATE INDEX` (unique + partial), `CREATE VIEW`, `CREATE TRIGGER`, `DROP TABLE/INDEX/VIEW/TRIGGER`, `ALTER TABLE RENAME TO` (catalog + schema move with index/trigger attachment and FK reference rewriting), `ALTER TABLE ADD COLUMN` (with DEFAULT back-fill), `ALTER TABLE RENAME COLUMN` (rewrites the table's CREATE statement, other tables' REFERENCES clauses, indexes, triggers and views), `ALTER TABLE DROP COLUMN` (validates SQLite's restrictions and physically rewrites every row)
- **DML**: `INSERT` (with `OR REPLACE/IGNORE`, `... RETURNING`, `UPSERT`), `UPDATE`, `DELETE` (with `RETURNING`)
- **UPSERT**: `INSERT ... ON CONFLICT (cols) DO NOTHING / DO UPDATE SET ... [WHERE ...]` with `excluded.*` references (SQLite semantics)
- **RETURNING**: `INSERT/UPDATE/DELETE ... RETURNING * | exprs` on all three write paths
- **CHECK + NOT NULL constraints**: enforced on INSERT, UPDATE, and UPSERT merges (column-level and table-level)
- **FOREIGN KEY constraints**: enforced when `PRAGMA foreign_keys = ON` (default off, like SQLite) — child-side checks on INSERT/UPDATE, parent-side checks on DELETE with `ON DELETE RESTRICT / CASCADE (recursive) / SET NULL / SET DEFAULT`, composite keys, implicit-PK references, and index maintenance on cascaded deletes
- **Implicit UNIQUE indexes**: column/table-level UNIQUE and non-rowid PKs create `sqlite_autoindex_*` (actually enforced)
- **Subqueries**: uncorrelated scalar / `IN (SELECT ...)` / `EXISTS (SELECT ...)` — executed once per statement, arbitrarily nested
- **Queries**: `SELECT` with `DISTINCT`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`
- **Joins**: `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, `NATURAL`, with `ON` / `USING`
- **Set operations**: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- **CTEs**: `WITH` (non-recursive) and `WITH RECURSIVE` — full execution
- **Window functions**: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `SUM() OVER (...)`, etc.
- **Aggregates**: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT`, with `DISTINCT`
- **Expressions**: arithmetic, string concat (`||`), bitwise, `CASE WHEN`, `CAST`, `LIKE`, `GLOB`, `BETWEEN`, `IN`, `IS NULL`, `IS`, etc.
- **Functions**: `ABS`, `LENGTH`, `LOWER`, `UPPER`, `TRIM`, `REPLACE`, `SUBSTR`, `COALESCE`, `NULLIF`, `IIF`, `ROUND`, `RANDOM`, `HEX`, `TYPEOF`, `INSTR`, `PRINTF`, scalar `MIN`/`MAX`, math functions, and more
- **JSON1**: `json()`, `json_extract()` (incl. `$[n]`, `$.a.b`, `[#-n]` paths, unicode escapes, surrogate pairs), `json_valid()`, `json_type()`, `json_quote()`, `json_array()`, `json_object()`, `json_array_length()`, `json_insert()`, `json_replace()`, `json_set()`, `json_remove()`, `json_patch()` (RFC 7396)
- **Date/time (full SQLite compatibility)**: `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`, `strftime()`, `timediff()` with all modifiers (`+N days/months/years`, `start of month/year/day`, `end of month`, `weekday N`, `unixepoch`, `localtime`/`utc`, `subsec`) — a faithful port of SQLite's `date.c`
- **Transactions**: `BEGIN`, `COMMIT`, `ROLLBACK` (auto-commit per statement; full transaction semantics still being wired up)
- **Pragmas**: `PRAGMA foreign_keys = ON/OFF` (honored); others parse and are accepted as no-ops

### Storage

- **Page format**: 4 KiB pages (SQLite's default since 3.12, configurable 512–64 KiB via `PRAGMA page_size`), 100-byte file header on page 0
- **B+tree**: clustered table B+tree (key = rowid) and index B+tree sorted by (key, rowid) with an order-preserving key encoding — O(log N) index seeks, prefix lookups for composite indexes, and range scans
- **Overflow chains**: rows larger than a page spill the payload tail to a linked chain of overflow pages (SQLite's overflow-cell layout: local prefix + first chain page) — megabyte BLOBs/TEXTs round-trip exactly, and `SELECT` streams them without buffering the chain
- **Index range scans**: `WHERE indexed_col > ?` / `BETWEEN` plans an `IndexRange` (index seek + fetch only matching rows)
- **Append-mode splits**: right-edge inserts keep the old leaf 100% full (SQLite's `balance_quick` behavior) — sequential loads fill pages ~2x denser than naive mid-splits
- **Page recycling**: DELETE unlinks empty leaves onto the pager freelist; new allocations reuse freelist pages before growing the file
- **Pager**: LRU cache, freelist, page allocation, dirty page tracking
- **WAL**: write-ahead log with CRC32 checksums, salt-based recovery, frame-level integrity
- **MVCC**: snapshot isolation via WAL frame indexing (foundation in place)
- **Row codec v2** (`RSQLDB02`): size-classed integers (1–9 bytes), LEB128 text/blob lengths, and rowid-alias elision — `id INTEGER PRIMARY KEY` is stored as a 1-byte marker materialized from the B+tree key, like SQLite's record format

### Tooling

- **CLI shell**: `rustqlite-cli` with table/JSON/CSV/line output modes, dot commands
- **HTTP/JSON server**: `rustqlite-server` with `/query`, `/execute`, `/health` endpoints
- **Benchmarks**: criterion-based benchmarks against rusqlite (SQLite) for point lookups, range scans, inserts, joins

### Plugin system (SQLite-style extensions)

- **User functions**: scalar (`create_function`) and aggregate (`create_aggregate`) functions in safe Rust — planner-visible, GROUP BY-integrated, arity-checked, built-ins protected from shadowing
- **Collations**: `NOCASE` / `RTRIM` built-ins plus user-defined sequences (`create_collation`), honored by `ORDER BY … COLLATE` and comparison operators
- **Virtual tables**: `CREATE VIRTUAL TABLE … USING module(...)` with SQLite's full callback protocol — `xCreate`/`xConnect`/`xBestIndex` constraint pushdown, cursors, and writable modules (`xUpdate`) — persisting across reopen like SQLite's runtime modules
- **Page codecs**: pluggable page encode/decode (`PRAGMA codec`), the SEE/ZIPVFS-style hook — XOR codec included as a working example with file markers and safe-refuse-on-wrong-codec
- **Dynamic extensions in any language**: compile against `include/rustqlite_ext.h`, export `rustqlite_extension_init`, load with `Database::load_extension` — working examples in **C, C++, Zig, and Rust** (`plugins/`)
- **SQLite-shaped C ABI**: the `rustqlite_*` family (`open`/`exec`/`prepare_v2`/`step`/`bind`/`column`/`load_extension`, …) mirroring `sqlite3_*` argument order and semantics — the binding layer for future sqlx support
- **Prepared statements** (`Database::prepare` + `Statement::bind/step/reset`): parsed and planned once, rebindable, and **streaming** — scans/ranges/filters/projections/limits (and vtab scans) deliver rows in batches without materializing the result set

See [PLUGINS.md](PLUGINS.md) for the full guide.

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

- **100% safe Rust** — no FFI, no lifetime-erased handles, trivially
  cross-compilable
- **Faster than sqlx-sqlite**: executes inline in the async task instead
  of ferrying every command and row across a dedicated worker thread +
  FFI — 1.5–2.8× on point lookups / scans / aggregates, 3.7× in
  transactions, 18× on `fetch()` streams (see `examples/bench_sqlx_native.rs`)
- **SQLite snapshot isolation between connections**: readers never see
  uncommitted writes (they wait, up to the busy timeout, then get
  `SQLITE_BUSY` — exactly like SQLite); read-only transactions never
  block readers; a dropped connection rolls back whatever transaction it
  left open, so one connection can never wedge the pool
- **URL scheme**: `rustqlite://app.db`, `rustqlite://:memory:?cache=shared`,
  `mode=rwc` / `immutable` options — drop-in-shaped for sqlx-style config

See [`src/sqlx_driver` module docs](src/sqlx_driver/mod.rs) for the full
guide (URL formats, isolation model, tuning knobs).

### sqlx & sea-orm compatibility (drop-in `libsqlite3`)

- **`compat/`** exports the real `sqlite3_*` C ABI (124 symbols) on the engine and ships a drop-in `libsqlite3-sys` replacement, so **unmodified crates.io sqlx 0.9 and sea-orm 2.0 run on rustqlite** via one `[patch.crates-io]` line
- SQLite-exact error messages + extended result codes (`SQLITE_CONSTRAINT_UNIQUE`, `SQLITE_MISMATCH`, …) — what sqlx's `error_kind()` and sea-orm's `DbErr` classify on
- Full UPDATE constraint semantics: sequential unique-index checking (ratchets pass, swaps conflict), `OR IGNORE`/`OR REPLACE`, rowid moves (`UPDATE t SET id = X`), collation-aware (NOCASE) unique probes, atomic statement aborts

See [compat/README.md](compat/README.md) and [docs/SQLX_COMPAT.md](docs/SQLX_COMPAT.md).

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design document.

The engine is structured in five layers, each with a clear contract:

```
┌──────────────────────────────────────────────┐
│  API (Database, Connection)                   │  ← user-facing
├──────────────────────────────────────────────┤
│  Executor (Volcano-style, collect-all)        │
├──────────────────────────────────────────────┤
│  Planner (AST → logical plan)                 │
├──────────────────────────────────────────────┤
│  SQL (lexer, parser, AST)                     │
├──────────────────────────────────────────────┤
│  Schema (catalog: tables, indexes, views)     │
├──────────────────────────────────────────────┤
│  Storage (pager, B+tree, WAL, MVCC)           │  ← disk I/O
└──────────────────────────────────────────────┘
```

## Performance

Head-to-head vs SQLite (rusqlite with bundled SQLite), `cargo run --release
--example bench_compare`, measured 2026-09-02 after the overflow-page +
fused-scan sprint (every row at parity or faster):

| Workload                              | rustqlite   | SQLite     | Ratio            |
|---------------------------------------|-------------|------------|------------------|
| Single-row inserts (auto-commit, 1k)  | **840 µs**  | 1.89 ms    | **2.25x faster** |
| INSERT in txn (100k rows)             | **79.9 ms** | 129 ms     | **1.62x faster** |
| Multi-row VALUES (10k rows)           | **4.26 ms** | 6.22 ms    | **1.46x faster** |
| Point lookup by rowid (1k ops)        | **291 µs**  | 373 µs     | **1.28x faster** |
| Range scan (10 rows)                  | **945 ns**  | 1.53 µs    | **1.62x faster** |
| Range scan (1000 rows)                | **67 µs**   | 107 µs     | **1.59x faster** |
| Full scan + COUNT with filter         | **404 µs**  | 470 µs     | **1.16x faster** |
| Aggregate (SUM/AVG/MIN/MAX)           | **683 µs**  | 1.18 ms    | **1.73x faster** |
| GROUP BY (100 buckets)                | **766 µs**  | 1.84 ms    | **2.40x faster** |
| Indexed point lookup (1k ops)         | **336 µs**  | 537 µs     | **1.60x faster** |
| 2-table join (PK filter)              | **2.24 µs** | 3.00 µs    | **1.34x faster** |
| 3-table join (PK filter, 50 out)      | **15.5 µs** | 22.2 µs    | **1.43x faster** |
| 2-table join + GROUP BY               | **1.73 ms** | 2.88 ms    | **1.66x faster** |
| UPDATE by PK (1k ops)                 | **1.66 ms** | 1.98 ms    | **1.19x faster** |
| UPDATE range (5k rows, indexed)       | **1.11 ms** | 1.14 ms    | **1.03x faster** |
| DELETE by PK (1k ops)                 | **611 µs**  | 1.39 ms    | **2.27x faster** |
| Mixed 80/20 read/write (5k ops)       | **1.94 ms** | 2.54 ms    | **1.31x faster** |
| DB file size (10k rows)               | **262 KB**  | 262 KB     | **byte-exact**   |
| Peak RSS (100k insert + count)        | **32.8 MB** | 35.5 MB    | **0.92x**        |

Every engine benchmark row leads or matches (steady-state probes in
GAP_ANALYSIS.md confirm each; the criterion harness measures concurrent
reads, `cargo bench --bench sqlite_comparison`).

### sqlx driver vs sqlx-sqlite (same sqlx 0.9 API)

`cargo run --release --example bench_sqlx_native --features sqlx` —
the native Rust driver against sqlx-sqlite (C SQLite) through the
identical sqlx API and pool options:

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

### Where we win

- **OLTP inserts**: a byte-level fast-path scanner executes single-row
  literal `INSERT ... VALUES (...)` without building tokens, an AST, or a
  plan — and `:memory:` databases skip per-statement file writes entirely
  (lazy write-back). Even with unique SQL text per statement (worst case
  for caching), we beat SQLite's re-prepare cost.
- **Concurrency**: multiple readers run truly in parallel on shared pages
  (page-level locks + interior-mutability pager); SQLite serializes
  everything through its connection mutex. Through the sqlx driver the
  same 8-way read workload is 2.8× sqlx-sqlite, and 1-writer/7-reader
  is 2× — with snapshot isolation, no dirty reads
  (`examples/probe_dirty_read.rs`).
- **UPDATE range / index scans**: the `IndexRange` plan node seeks the index
  and touches only matching rows; SQLite's planner picks a full table walk
  on this workload shape.
- **Rowid range scans**: `WHERE id BETWEEN ? AND ?` runs a dedicated fast
  path — no pipeline setup, binary-searched leaf descent, and an early
  stop at the first cell past the range end (previously every leaf right
  of the range was still visited). 100- and 1000-row ranges now beat
  SQLite; 10-row ranges cut from 39 µs to ~10 µs.
- **Filtered joins**: IndexNestedLoopJoin + a warm statement cache beats
  SQLite's prepared-statement path on point-filtered joins.
- **Bulk inserts**: BTREE_APPEND right-most descent + append-mode splits +
  codec v2 (rows are ~40% smaller than v1) keep sequential loads dense.
- **File size**: byte-exact with SQLite on identical workloads (4 KiB
  default pages, ~100% sequential-load page fill, overflow chains).
- **Streaming**: `fetch()` / `Statement::step` deliver rows in batches from
  a resumable fused Filter-over-Scan driver with selective decode —
  non-matching rows decode only the predicate's columns, and `LIMIT k`
  stops the walk at the k-th match.

### Where the remaining deltas are

Every `bench_compare` row now leads or matches; the residual deltas are
elsewhere and shrinking (full ledger in GAP_ANALYSIS.md):

- **Binary size (+0.4 MB)**: mimalloc adds ~140 KiB of code but buys
  1.5–2.1× on write-heavy paths; builds with `default-features = false`
  drop it.
- **8-conn mixed R/W 80/20 (1.05x)**: at high write-fan-out the writer
  gate + commit fsync dominates; reads stay 2.8× throughout.
- **`UPDATE range` (1.03x)**: parity at the bench's size; wins grow with
  table size (payload-patch fast path avoids full row re-encode).

Run the benchmarks yourself:

```bash
cargo bench --bench sqlite_comparison  # full head-to-head
cargo bench --bench point_lookup -- --quick
cargo bench --bench range_scan  -- --quick
cargo bench --bench insert      -- --quick
cargo bench --bench join        -- --quick
```

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

See [`examples/`](examples/):

- `basic.rs`: create, insert, query, update, delete, aggregates, group by
- `transaction.rs`: atomic transfer between accounts
- `batch.rs`: bulk insert + aggregation benchmarks

```bash
cargo run --example basic
cargo run --example transaction
cargo run --example batch
```

## Testing

The test matrix is modeled on SQLite's own testing methodology
([sqlite.org/testing.html](https://www.sqlite.org/testing.html)): crash and
power-loss simulation (child processes aborting at every statement
boundary), I/O-error and OOM fault injection, database-corruption fuzzing,
SQL mutation fuzzing with differential verification against real SQLite,
`PRAGMA integrity_check`, boundary values, regression tests, concurrency
stress, and SQL Logic Tests. See [TESTING.md](TESTING.md) for the full
matrix and how to run each harness.

```bash
cargo test                                              # the whole matrix
cargo test --features sqlx --test sqlx_driver           # native sqlx driver
cargo test --test crash_recovery                        # crash simulation
cargo test --features oom-injection --test oom_fault    # OOM injection
cargo run --release --example bench_compare             # vs SQLite
cargo run --release --example bench_sqlx_native --features sqlx
```

## Limitations

Known gaps (shrinking — see `GAP_ANALYSIS.md` for the full ledger):

- **Concurrent access**: page-level MRMW reads (3.1× SQLite's
  concurrent-read throughput) plus the sqlx driver's snapshot isolation
  (no dirty reads, readers never blocked by read-only transactions).
  Multi-connection single-writer concurrency is SQLite-equivalent;
  multiple concurrent writers still serialize through the write gate.
- **Numeric precision**: `AVG` rounds to 10 decimals; some edge cases in
  real formatting differ from SQLite.
- **Compat surface**: `sqlite3_serialize`/`deserialize`, preupdate hooks,
  and unlock-notify are stubs in `compat/`; `sqlite_master` DDL text and a
  few `PRAGMA` result shapes continue to be tightened via differential
  tests (`tests/pragma_introspect.rs`, `tests/update_from_collate.rs`).

## License

MIT OR Apache-2.0.

## Acknowledgments

The architecture is heavily inspired by SQLite (page format, B+tree layout, WAL design) and PostgreSQL (MVCC concepts, planner structure). The [SQLite Database File Format](https://www.sqlite.org/fileformat.html) document was an invaluable reference.
