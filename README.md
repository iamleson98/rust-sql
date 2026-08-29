# rustqlite

A from-scratch embedded SQL database engine written in pure Rust, modeled after SQLite but designed for cleaner code and competitive performance.

> **Status**: This is an educational / proof-of-concept implementation. It implements a strong subset of SQLite's SQL surface, with a clean layered architecture. It is **not** production-ready — see [Limitations](#limitations).

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

- **DDL**: `CREATE TABLE`, `CREATE INDEX` (unique + partial), `CREATE VIEW`, `CREATE TRIGGER`, `DROP TABLE/INDEX/VIEW/TRIGGER`, `ALTER TABLE` (parsed, not fully implemented)
- **DML**: `INSERT` (with `OR REPLACE/IGNORE`, `... RETURNING`, `UPSERT`), `UPDATE`, `DELETE` (with `RETURNING`)
- **UPSERT**: `INSERT ... ON CONFLICT (cols) DO NOTHING / DO UPDATE SET ... [WHERE ...]` with `excluded.*` references (SQLite semantics)
- **RETURNING**: `INSERT/UPDATE/DELETE ... RETURNING * | exprs` on all three write paths
- **CHECK + NOT NULL constraints**: enforced on INSERT, UPDATE, and UPSERT merges (column-level and table-level)
- **Implicit UNIQUE indexes**: column/table-level UNIQUE and non-rowid PKs create `sqlite_autoindex_*` (actually enforced)
- **Subqueries**: uncorrelated scalar / `IN (SELECT ...)` / `EXISTS (SELECT ...)` — executed once per statement, arbitrarily nested
- **Queries**: `SELECT` with `DISTINCT`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`
- **Joins**: `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, `NATURAL`, with `ON` / `USING`
- **Set operations**: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- **Subqueries**: scalar subqueries, `IN (subquery)`, `EXISTS (subquery)` (parsed; some forms not yet executed)
- **CTEs**: `WITH` (non-recursive and recursive, parsed)
- **Window functions**: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `SUM() OVER (...)`, etc.
- **Aggregates**: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT`, with `DISTINCT`
- **Expressions**: arithmetic, string concat (`||`), bitwise, `CASE WHEN`, `CAST`, `LIKE`, `GLOB`, `BETWEEN`, `IN`, `IS NULL`, `IS`, etc.
- **Functions**: `ABS`, `LENGTH`, `LOWER`, `UPPER`, `TRIM`, `REPLACE`, `SUBSTR`, `COALESCE`, `NULLIF`, `IIF`, `ROUND`, `RANDOM`, `HEX`, `TYPEOF`, `INSTR`, `PRINTF`, scalar `MIN`/`MAX`, math functions, and more
- **Date/time (full SQLite compatibility)**: `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`, `strftime()`, `timediff()` with all modifiers (`+N days/months/years`, `start of month/year/day`, `end of month`, `weekday N`, `unixepoch`, `localtime`/`utc`, `subsec`) — a faithful port of SQLite's `date.c`
- **Transactions**: `BEGIN`, `COMMIT`, `ROLLBACK` (auto-commit per statement; full transaction semantics still being wired up)
- **Pragmas**: `PRAGMA` (most are no-ops; honored: none yet)

### Storage

- **Page format**: 16 KiB pages (configurable), 100-byte file header on page 0
- **B+tree**: clustered table B+tree (key = rowid) and index B+tree sorted by (key, rowid) with an order-preserving key encoding — O(log N) index seeks, prefix lookups for composite indexes, and range scans
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

Benchmarks vs SQLite (rusqlite with bundled SQLite), in-memory database,
steady-state (statements warm). Full harness: `cargo run --release --example bench_compare`.

| Workload                          | rustqlite | SQLite  | Ratio           |
|-----------------------------------|-----------|---------|-----------------|
| UPDATE range (indexed)            | 21 µs     | 1.24 ms | **59x faster**  |
| 3-table join, filter by PK        | 3.6 µs    | 56 µs   | **15x faster**  |
| 2-table join, filter by PK        | 4.2 µs    | 24 µs   | **5.8x faster** |
| UPDATE by PK (1k ops)             | 1.25 ms   | 1.82 ms | **1.5x faster** |
| Aggregates (SUM/AVG/MIN/MAX)      | 555 µs    | 1.18 ms | **2.1x faster** |
| Single-row inserts (1k, autocommit)| 981 µs   | 1.80 ms | **1.8x faster** |
| 100k inserts in BEGIN/COMMIT      | 116 ms    | 128 ms  | **1.1x faster** |
| GROUP BY (100 buckets)            | 1.92 ms   | 1.88 ms | parity          |
| Indexed point lookup (1k ops)     | 539 µs    | 526 µs  | parity          |
| Range scan (1000 rows)            | 167 µs    | 108 µs  | 1.5x slower     |
| Point lookup by rowid (1k ops)    | 758 ns/op | 362 ns/op | 2.1x slower   |
| DELETE by PK (1k ops)             | 4.6 ms    | 1.35 ms | 3.4x slower     |
| DB file size (10k rows)           | 328 KB    | 262 KB  | 1.25x larger    |
| Stripped binary size              | 1.28 MB   | 1.99 MB | **0.64x**       |

### Where we win

- **UPDATE range / index scans**: the `IndexRange` plan node seeks the index
  and touches only matching rows; SQLite's planner picks a full table walk
  on this workload shape.
- **Filtered joins**: IndexNestedLoopJoin + a warm statement cache beats
  SQLite's prepared-statement path on point-filtered joins.
- **Bulk inserts**: BTREE_APPEND right-most descent + append-mode splits +
  codec v2 (rows are ~40% smaller than v1) keep sequential loads dense.
- **File size**: codec v2 + ~100% page fill for sequential loads lands
  within 1.25x of SQLite's record format (was 3.5x).
- **Binary size**: no libc regex/pcre, no ICU, no optional extensions.

### Where SQLite still wins

- **Point lookups / small scans**: the rusqlite-style API materializes an
  owned `Vec<Row>` per query (2–3 allocations); SQLite's step/column API
  avoids that. A cursor/callback API is the fix.
- **DELETE by PK**: we default to per-statement durability (flush+fsync
  per statement); the harness runs SQLite in WAL + `synchronous=OFF`.
- **Full scans with filters**: per-row predicate evaluation still resolves
  column names (string compares); resolving to indices like the GROUP BY
  path now does is the fix.

Run the benchmarks yourself:

```bash
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

## Limitations

This is a proof-of-concept. Known gaps:

- **Streaming**: The executor materializes all rows in memory (collect-all model). A pull-based streaming executor would be needed for large result sets.
- **Index usage**: The planner parses indexes but does not yet use them for `WHERE col = ?` lookups. Only `WHERE rowid = ?` could be optimized (and isn't yet).
- **Subquery execution**: Scalar subqueries and `EXISTS` parse but return errors at execution time.
- **Foreign keys**: Parsed but not enforced.
- **Triggers**: Stored in the catalog but not fired.
- **Views**: Stored in the catalog but not expanded in queries.
- **UPDATE/DELETE without INTEGER PRIMARY KEY**: Not yet supported (requires rowid tracking through scans).
- **Concurrent access**: The pager takes a single `&mut` — there's no MVCC visibility check yet, just the infrastructure.
- **Numeric precision**: `AVG` rounds to 10 decimals; some edge cases in real formatting differ from SQLite.
- **Date/time**: Minimal stubs — `CURRENT_TIMESTAMP` always returns the Unix epoch.
- **Collations**: Only `BINARY` is honored.
- **Pragmas**: All no-ops.
- **Correlated subqueries**: clean "unsupported" errors (uncorrelated ones work).
- **UPDATE/DELETE without INTEGER PRIMARY KEY**: not supported (rowid tracking).

## License

MIT OR Apache-2.0.

## Acknowledgments

The architecture is heavily inspired by SQLite (page format, B+tree layout, WAL design) and PostgreSQL (MVCC concepts, planner structure). The [SQLite Database File Format](https://www.sqlite.org/fileformat.html) document was an invaluable reference.
