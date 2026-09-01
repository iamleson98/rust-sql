# rustqlite

A from-scratch embedded SQL database engine written in pure Rust, modeled after SQLite but designed for cleaner code and competitive performance.

> **Status**: Educational / proof-of-concept core, now with a full SQLite-style **plugin system** (functions, aggregates, collations, virtual tables, page codecs) loadable from C, C++, Zig, and Rust — see [PLUGINS.md](PLUGINS.md). Still not production-ready — see [Limitations](#limitations).

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

### Plugin system (SQLite-style extensions)

- **User functions**: scalar (`create_function`) and aggregate (`create_aggregate`) functions in safe Rust — planner-visible, GROUP BY-integrated, arity-checked, built-ins protected from shadowing
- **Collations**: `NOCASE` / `RTRIM` built-ins plus user-defined sequences (`create_collation`), honored by `ORDER BY … COLLATE` and comparison operators
- **Virtual tables**: `CREATE VIRTUAL TABLE … USING module(...)` with SQLite's full callback protocol — `xCreate`/`xConnect`/`xBestIndex` constraint pushdown, cursors, and writable modules (`xUpdate`) — persisting across reopen like SQLite's runtime modules
- **Page codecs**: pluggable page encode/decode (`PRAGMA codec`), the SEE/ZIPVFS-style hook — XOR codec included as a working example with file markers and safe-refuse-on-wrong-codec
- **Dynamic extensions in any language**: compile against `include/rustqlite_ext.h`, export `rustqlite_extension_init`, load with `Database::load_extension` — working examples in **C, C++, Zig, and Rust** (`plugins/`)
- **SQLite-shaped C ABI**: the `rustqlite_*` family (`open`/`exec`/`prepare_v2`/`step`/`bind`/`column`/`load_extension`, …) mirroring `sqlite3_*` argument order and semantics — the binding layer for future sqlx support
- **Prepared statements** (`Database::prepare` + `Statement::bind/step/reset`): parsed and planned once, rebindable, and **streaming** — scans/ranges/filters/projections/limits (and vtab scans) deliver rows in batches without materializing the result set

See [PLUGINS.md](PLUGINS.md) for the full guide.

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
--example bench_compare`, measured 2026-08-30 after the point-lookup /
UPDATE / JOIN / WAL sprint:

| Workload                              | rustqlite   | SQLite     | Ratio            |
|---------------------------------------|-------------|------------|------------------|
| Single-row inserts (auto-commit, 1k)  | **770 µs**  | 1.78 ms    | **2.3x faster**  |
| INSERT in txn (100k rows)             | **74.6 ms** | 128 ms     | **1.7x faster**  |
| Point lookup by rowid (1k ops)        | **331 µs**  | 386 µs     | **1.17x faster** |
| Range scan (10 rows)                  | **1.9 µs**  | 3.9 µs     | **2.1x faster**  |
| Range scan (1000 rows)                | **102 µs**  | 111 µs     | **1.09x faster** |
| Full scan + COUNT with filter         | **333 µs**  | 534 µs     | **1.6x faster**  |
| Aggregate (SUM/AVG/MIN/MAX)           | **631 µs**  | 1.30 ms    | **2.1x faster**  |
| GROUP BY (100 buckets)                | **1.89 ms** | 2.05 ms    | **1.08x faster** |
| 2-table join (PK filter)              | **28 µs**   | 42 µs      | **1.5x faster**  |
| UPDATE by PK (1k ops)                 | **1.53 ms** | 1.80 ms    | **1.17x faster** |
| UPDATE range (5k rows, indexed)       | 1.27 ms     | 1.24 ms    | parity           |
| DELETE by PK (1k ops)                 | **505 µs**  | 1.32 ms    | **2.6x faster**  |
| Mixed 80/20 read/write (5k ops)       | **2.1 ms**  | 2.4 ms     | **1.15x faster** |
| Concurrent reads (8 threads, criterion)| **4.8 ms** | 15.1 ms    | **3.1x faster**  |
| File commit, WAL/NORMAL (per txn)     | **17.9 µs** | 27.7 µs    | **1.55x faster** |
| File commit, delete mode (per txn)    | **17.8 µs** | 130 µs     | **7.3x faster**  |
| Peak RSS (100k insert + count)        | **30.3 MB** | 33.1 MB    | **0.92x**        |
| Stripped binary size                  | **1.96 MB** | 2.01 MB    | **0.97x**        |

17 of 23 benchmark rows lead; the remainder are at parity or within
run-to-run variance (steady-state probes show parity or better on each —
see GAP_ANALYSIS.md). Concurrent reads are measured with the criterion
harness (`cargo bench --bench sqlite_comparison`).

### Where we win

- **OLTP inserts**: a byte-level fast-path scanner executes single-row
  literal `INSERT ... VALUES (...)` without building tokens, an AST, or a
  plan — and `:memory:` databases skip per-statement file writes entirely
  (lazy write-back). Even with unique SQL text per statement (worst case
  for caching), we beat SQLite's re-prepare cost.
- **Concurrency**: multiple readers run truly in parallel on shared pages
  (page-level locks + interior-mutability pager); SQLite serializes
  everything through its connection mutex.
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
- **File size**: codec v2 + ~100% page fill for sequential loads lands
  within 1.13x of SQLite's record format (was 3.5x).
- **Binary size**: no libc regex/pcre, no ICU, no optional extensions.

### Where SQLite still wins

- **Full inner joins (1.3x)**: scan-side row materialization (one Vec per
  row) is the remainder of the join gap; a column-block scan would close it.
- **Tiny range scans (2.9x on 10-row ranges)**: SQLite's prepared VDBE
  program still has ~3 µs less fixed cost than our fast path; the residual
  is statement-cache lookup + row Vec allocation.
- **Indexed point lookups (1.4x)**: the index path builds both the index
  key and the result row per hit; SQLite's OP_SeekGE/OP_IdxRowid pair does
  it with precompiled registers.
- **Full scans with filters**: per-row predicate evaluation still resolves
  column names (string compares); resolving to indices like the GROUP BY
  path now does is the fix.

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
cargo test --test crash_recovery                        # crash simulation
cargo test --features oom-injection --test oom_fault    # OOM injection
cargo run --release --example bench_compare             # vs SQLite
```

## Limitations

This is a proof-of-concept. Known gaps:

- **Streaming**: The executor materializes all rows in memory (collect-all
  model). A pull-based streaming executor would be needed for large result
  sets.
- **Correlated subqueries**: clean "unsupported" errors (uncorrelated
  scalar / IN / EXISTS subqueries work).
- **ALTER TABLE RENAME COLUMN / DROP COLUMN**: parsed, rejected with a
  clear error (RENAME TO and ADD COLUMN are implemented).
- **Concurrent access**: page-level MRMW reads work (3.1x SQLite's
  concurrent-read throughput); full MVCC visibility wiring is still
  infrastructure-only.
- **Numeric precision**: `AVG` rounds to 10 decimals; some edge cases in
  real formatting differ from SQLite.
- **Collations**: `BINARY`/`NOCASE`/`RTRIM` plus user-registered sequences work in ORDER BY and comparisons; index collations (COLLATE in CREATE INDEX / column definitions) are not yet used by the planner for index scans.
- **Pragmas**: Only `foreign_keys` changes behavior; the rest are no-ops.
- **WAL/MVCC**: the log and snapshot machinery exists (CRC32 frames, salt
  recovery) but readers do not yet serve queries from WAL frames.

## License

MIT OR Apache-2.0.

## Acknowledgments

The architecture is heavily inspired by SQLite (page format, B+tree layout, WAL design) and PostgreSQL (MVCC concepts, planner structure). The [SQLite Database File Format](https://www.sqlite.org/fileformat.html) document was an invaluable reference.
