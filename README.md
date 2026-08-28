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

- **Page format**: 4 KiB pages (configurable), 100-byte file header on page 0
- **B+tree**: clustered table B+tree (key = rowid) and index B+tree sorted by (key, rowid) with an order-preserving key encoding — O(log N) index seeks, prefix lookups for composite indexes, and range scans
- **Index range scans**: `WHERE indexed_col > ?` / `BETWEEN` plans an `IndexRange` (index seek + fetch only matching rows)
- **Pager**: LRU cache, freelist, page allocation, dirty page tracking
- **WAL**: write-ahead log with CRC32 checksums, salt-based recovery, frame-level integrity
- **MVCC**: snapshot isolation via WAL frame indexing (foundation in place)
- **Row codec**: compact binary encoding with type tags

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

Preliminary benchmarks vs SQLite (rusqlite with bundled SQLite) on an in-memory database:

| Workload              | rustqlite  | SQLite (rusqlite) | Ratio    |
|-----------------------|-----------|-------------------|----------|
| Point lookup (rowid)  | 12.4 µs   | 1.57 µs           | 7.9x slower |
| Range scan (4000 rows)| 19.6 µs   | 342 µs            | **17x faster** |
| Insert (1000 rows)    | 11.3 ms   | 1.5 ms            | 7.5x slower |
| Join (5 rows out)     | 3.57 ms   | 3.9 µs            | ~900x slower |

### Why these numbers

- **Range scan is faster**: SQLite has more per-row overhead (VDBE interpretation, type coercion, etc.). For bulk scans, our simpler executor wins.
- **Point lookup is slower**: We don't yet use the rowid B+tree for `WHERE rowid = ?` — we always scan. Adding rowid point lookup would close most of this gap.
- **Insert is slower**: We call `pager.flush()` after every INSERT, writing through to disk. SQLite uses WAL + group commit. Batching inserts in a transaction would close the gap.
- **Join is slower**: Our nested-loop join materializes the entire inner side for each outer row. A hash join or streaming nested-loop would close the gap.

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
