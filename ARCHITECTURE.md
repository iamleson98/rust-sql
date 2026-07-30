# Architecture

This document describes the design of rustqlite, the trade-offs made, and the rationale behind each decision.

## Overview

rustqlite is a layered embedded SQL database engine written in pure Rust. The architecture mirrors SQLite's (page format, B+tree, WAL, SQL front-end / back-end split) but is implemented from scratch with cleaner separation of concerns and modern Rust idioms.

```
┌──────────────────────────────────────────────────────┐
│  Public API                                           │
│  Database, Connection, Params                         │
├──────────────────────────────────────────────────────┤
│  Executor                                             │
│  Plan → rows (collect-all model)                     │
│  Operators: Scan, Filter, Project, Sort, Limit,      │
│    Aggregate, Window, Join, Distinct, Union,          │
│    Intersect, Except, RowidLookup,                    │
│    Insert, Update, Delete                             │
├──────────────────────────────────────────────────────┤
│  Planner                                              │
│  AST → logical Plan                                   │
│  Name resolution, aggregate rewriting, basic          │
│    index hints                                        │
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
│  Pager (LRU cache, freelist, file I/O)                │
│    → B+tree (table + index, leaf + interior)         │
│    → WAL (CRC32, salt, recovery)                      │
│    → MVCC (snapshot isolation via WAL frames)         │
│    → Row codec (compact binary)                       │
└──────────────────────────────────────────────────────┘
```

Each layer has a single responsibility and a well-defined contract with the layer above. The dependencies flow strictly downward: the executor never touches the pager directly; it goes through the B+tree.

---

## Storage Layer

### Page Format

The database file is a sequence of fixed-size pages (default 4 KiB). Page 0 is special: it begins with a 100-byte file header, followed by the B+tree header for the schema table (which is always rooted at page 0).

**File header (100 bytes):**

| Offset | Size | Field                          |
|--------|------|--------------------------------|
| 0      | 8    | Magic: `"RSQLDB01"`            |
| 8      | 4    | Page size (LE u32)             |
| 12     | 4    | File change counter (LE u32)   |
| 16     | 4    | Database size in pages (LE u32)|
| 20     | 4    | Freelist head page (LE u32)    |
| 24     | 4    | Freelist page count (LE u32)   |
| 28     | 4    | Schema cookie (LE u32)         |
| 32     | 4    | Schema format version (1)      |
| 36     | 4    | Cache size hint                |
| 40     | 8    | Largest root b-tree page       |
| 48     | 4    | Text encoding (1=UTF-8)        |
| 52     | 4    | User version                   |
| 56     | 4    | Incremental vacuum mode        |
| 60     | 4    | Application ID                 |
| 64     | 28   | Reserved                       |
| 92     | 4    | Version-valid-for              |
| 96     | 4    | SQLite version magic           |

**B+tree page header (12 bytes):**

| Offset | Size | Field                              |
|--------|------|------------------------------------|
| 0      | 1    | Page type (0x05/0x0D/0x02/0x0A)    |
| 1      | 3    | Reserved (alignment)               |
| 4      | 2    | Number of cells (BE u16)           |
| 6      | 2    | Cell content area start (BE u16)   |
| 8      | 4    | Right-most child pointer (interior)|

Following the header is the cell pointer array (BE u16 each), then free space, then the cell content area (growing downward from the end of the page).

**Page types:**

- `0x0D` — Leaf table: cells are `(rowid, payload)` pairs
- `0x05` — Interior table: cells are `(left_child_page, key)` pairs
- `0x0A` — Leaf index: cells are `(key, rowid)` pairs
- `0x02` — Interior index: cells are `(left_child_page, key, rowid)` pairs

### Varint Encoding

Varints are big-endian, 1-9 bytes. The first 8 bytes use 7 bits each (with the high bit set as a continuation flag). The 9th byte (if present) uses all 8 bits. Total capacity: 7×8 + 8 = 64 bits — enough for any `u64`.

This matches SQLite's varint format exactly, so a rustqlite database could (in principle) be read by SQLite and vice versa, given compatible payload encodings.

### Pager

The pager owns:
- The database file handle
- An LRU cache of pages (`HashMap<PageId, Rc<RefCell<Page>>>` + ordering deque)
- The freelist (head page + count)
- The schema cookie
- The current page count

**Operations:**
- `get_page(id)`: cache lookup → disk read if miss → insert into cache (evicting LRU pages).
- `allocate_page()`: pop from freelist if non-empty, else extend the file.
- `free_page(id)`: push onto freelist (the freed page's first 4 bytes store the next freelist pointer).
- `flush()`: write all dirty pages back to disk, update the file header, fsync.

Pages are returned as `Rc<RefCell<Page>>` so that the B+tree can hold multiple references to the same page during splits and merges without copying.

**Cache eviction policy**: LRU. Dirty pages are not evicted (they're moved to the back of the LRU list and retried). This is a simple correctness-preserving policy; a production engine would have a write-back queue.

### B+tree

Two flavors:

**Table B+tree** (clustered index on rowid):
- Leaf cells: `varint(rowid) | varint(payload_len) | payload`
- Interior cells: `be_u32(left_child_page) | varint(key)`
- The rowid is the B+tree key; the payload is the encoded row.
- Point lookup: descend from root, comparing rowid at each interior node.
- Insert: descend to leaf, insert sorted. If the leaf overflows, split: redistribute cells, propagate the split key up. If the root splits, create a new root.
- Delete: descend to leaf, remove the cell. (We don't rebalance — underfull pages are left as-is, like SQLite.)
- Scan: in-order traversal of leaves.

**Index B+tree** (secondary index):
- Leaf cells: `varint(rowid) | key`
- Interior cells: `be_u32(left_child_page) | varint(rowid) | key`
- The key is the encoded index columns (concatenated `Value::encode` output).

The implementation is intentionally simple: no prefix compression, no suffix truncation, no sibling pointers. This trades some space and minor scan performance for clarity.

### WAL

The WAL is a separate file (`<db>-wal`) containing committed page writes that haven't been checkpointed into the main database file yet.

**WAL header (32 bytes):**

| Offset | Size | Field                          |
|--------|------|--------------------------------|
| 0      | 4    | Magic: `0x5253514C` ("RSQL")   |
| 4      | 4    | Format version (1)             |
| 8      | 4    | Page size (BE u32)             |
| 12     | 4    | Checkpoint sequence number     |
| 16     | 4    | Salt 1 (random)                |
| 20     | 4    | Salt 2 (random)                |
| 24     | 4    | Checksum 1                     |
| 28     | 4    | Checksum 2                     |

**Frame header (24 bytes) per frame:**

| Offset | Size | Field                          |
|--------|------|--------------------------------|
| 0      | 4    | Page ID (BE u32)               |
| 4      | 4    | Commit marker (non-zero = commit)|
| 8      | 4    | Salt 1 (copied from WAL header)|
| 12     | 4    | Salt 2 (copied from WAL header)|
| 16     | 4    | Checksum 1 (running)           |
| 20     | 4    | Checksum 2 (running)           |

The checksum is a running CRC32 over (page_id + commit_marker + page_data), seeded with the previous frame's checksum. A torn write at frame N invalidates frames N+1, N+2, ... — recovery stops at the first checksum mismatch.

**Recovery**: On open, read the WAL header, walk frames, verify salt matches the header and checksums chain correctly. The number of valid frames becomes `n_frames`.

**Checkpoint**: Apply all valid frames to the main database file, then reset the WAL.

### MVCC

MVCC provides snapshot isolation: each transaction sees a consistent snapshot of the database taken at the transaction's start TXID. Readers never block writers, and writers never block readers.

**Implementation strategy** (foundation in place, not fully wired):

- Each transaction has a monotonic TXID (global atomic counter).
- The WAL is the source of truth for committed writes; the main DB file is a checkpoint of the WAL.
- A snapshot at TXID `t` sees the latest version of each page from frames committed at TXID ≤ `t`.
- `VersionTracker` records, for each page, the WAL frame indices where it was written. To find the version of a page visible to a snapshot, binary-search the page's frame list for the largest frame ≤ the snapshot's frame count.

This is simpler than PostgreSQL's MVCC (which keeps tuple-level version chains) but gives snapshot isolation at the page level. For an embedded database, page-level MVCC is sufficient.

### Row Codec

Rows are encoded as a sequence of `Value::encode()` outputs, concatenated. Each value is:
- 1 byte tag (0=NULL, 1=INTEGER, 2=REAL, 3=TEXT, 4=BLOB)
- Payload:
  - INTEGER: 8 bytes LE i64
  - REAL: 8 bytes LE f64
  - TEXT: 4 bytes LE u32 length + UTF-8 bytes
  - BLOB: 4 bytes LE u32 length + raw bytes

Decoding pads missing columns with NULL (handles `ALTER TABLE ADD COLUMN`).

---

## SQL Layer

### Lexer

A hand-written state machine. Tracks line/column for error messages. Tokens:

- Keywords (case-insensitive, normalized to uppercase)
- Identifiers (case-sensitive)
- Double-quoted identifiers
- Integer literals (decimal, hex `0x...`)
- Float literals (with optional exponent)
- String literals (single-quoted, with `''` escape)
- Blob literals (`x'...'`)
- Parameters (`?`, `?N`, `:name`, `@name`, `$name`)
- Operators (multi-char first: `<=`, `>=`, `!=`, `<>`, `==`, `||`, `<<`, `>>`)
- Punctuation (`(`, `)`, `,`, `;`, `.`)

Comments: `-- line` and `/* block */`.

### Parser

Recursive descent with precedence climbing for binary operators. The parser produces an `ast::Statement` which is a faithful representation of the source SQL (minimal desugaring).

**Statement types:**
- `CREATE {TABLE | INDEX | VIEW | TRIGGER}`
- `DROP {TABLE | INDEX | VIEW | TRIGGER}`
- `INSERT` (with `OR REPLACE/IGNORE/FAIL/ABORT/ROLLBACK`, `VALUES`/`SELECT`/`DEFAULT VALUES`, `UPSERT`, `RETURNING`)
- `SELECT` (with `WITH`, `DISTINCT`, `FROM` with joins, `WHERE`, `GROUP BY`, `HAVING`, `WINDOW`, `ORDER BY`, `LIMIT`/`OFFSET`, set operations `UNION`/`INTERSECT`/`EXCEPT`)
- `UPDATE` (with `SET`, `FROM`, `WHERE`, `RETURNING`)
- `DELETE` (with `WHERE`, `RETURNING`, `ORDER BY`, `LIMIT`)
- `BEGIN` / `COMMIT` / `ROLLBACK`
- `PRAGMA`
- `ATTACH` / `DETACH`
- `VACUUM`
- `EXPLAIN`

**Expression grammar** (precedence, lowest to highest):
1. `OR`
2. `AND`
3. `=`, `!=`, `<`, `<=`, `>`, `>=`
4. `|` (bitwise OR)
5. `^` (bitwise XOR)
6. `&` (bitwise AND)
7. `<<`, `>>` (shifts)
8. `+`, `-`, `||` (concat)
9. `*`, `/`, `%`
10. unary `-`, `+`, `~`, `NOT`

Postfix operators: `COLLATE`, `IS [NOT] NULL`, `ISNULL`, `NOTNULL`, `[NOT] LIKE/GLOB/REGEXP/MATCH`, `[NOT] IN`, `[NOT] BETWEEN`, `FILTER (WHERE ...)`, `OVER (...)`.

Primary expressions: literals, parameters, column refs, function calls, `CASE`, `CAST`, `EXISTS`, `RAISE`, `(expr)`, `(subquery)`, `(row, row, ...)`.

### AST

The AST uses `Box` for recursive types to break size cycles. Variants:

- `Expr`: literals, parameters, columns, binary/unary ops, `BETWEEN`, `IN`, `LIKE`, `IS NULL`, `IS`, `Function` (with `FILTER` and `OVER`), `CASE`, `Row`, `Subquery`, `EXISTS`, `CAST`, `COLLATE`, `RAISE`.
- `Statement`: `Create`, `Drop`, `Insert`, `Select`, `Update`, `Delete`, `Begin`, `Commit`, `Rollback`, `Explain`, `Pragma`, `Attach`, `Detach`, `Vacuum`.

---

## Schema Layer

The catalog is an in-memory `HashMap` of name → `Arc<Table>` / `Arc<Index>` / `Arc<View>` / `Arc<Trigger>`. It is persisted as rows in the schema table (rooted at page 0), with columns `(type, name, tbl_name, rootpage, sql)`.

On `Database::open`, the schema table is scanned and each row is re-parsed to reconstruct the catalog. On any DDL, the schema cookie is bumped and the schema table is updated.

`Table` contains:
- Column definitions (name, affinity, declared type, constraints, defaults, generated columns)
- Root page ID
- `rowid_alias: Option<usize>` — the column index that is `INTEGER PRIMARY KEY` (and thus aliases the rowid)
- `without_rowid`, `strict` flags

---

## Planner

The planner converts an `ast::SelectStatement` into a `Plan` tree. It does:

1. **Name resolution**: table aliases are registered in a scope stack. (Currently minimal — the executor does most of the resolution at runtime.)
2. **Plan shape**: 
   - `Scan` → `Filter` (WHERE) → `Aggregate` (GROUP BY + aggs) → `Filter` (HAVING) → `Window` → `Distinct` → `Project` → `Sort` (ORDER BY) → `Limit`
3. **Aggregate rewriting**: When a SELECT has aggregates, the planner rewrites each `SUM(x)` etc. in the projection list to a column reference `__agg_N`, where N is the index of the aggregate in the `Aggregate` operator's output. This lets the Project reference the Aggregate's pre-computed results instead of re-evaluating the function.

The planner does **not** yet do:
- Predicate pushdown into scans
- Index selection (indexes are parsed and stored but not used for query planning)
- Join reordering
- Subquery decorrelation
- Common subexpression elimination

These are all straightforward to add given the existing structure.

---

## Executor

The executor walks a `Plan` tree and produces rows. It uses a **collect-all model**: each operator returns `ExecResult { columns, rows }` rather than a streaming iterator. This trades memory efficiency for code simplicity (no lifetime gymnastics with shared `&mut Pager`).

**Operators:**

| Operator     | Description                                              |
|--------------|----------------------------------------------------------|
| `Scan`       | B+tree scan of a table                                   |
| `Values`     | Literal rows (e.g. `SELECT 1, 2` or `VALUES (1,2),(3,4)`)|
| `Filter`     | Predicate evaluation per row                             |
| `Project`    | Column selection / expression evaluation                 |
| `Sort`       | Materialize + sort by multiple keys                      |
| `Limit`      | Skip + take                                              |
| `Aggregate`  | Group-by + aggregate accumulation                        |
| `Window`     | Partition + order + window function computation         |
| `Join`       | Nested-loop join (left × right, with condition)          |
| `Distinct`   | Deduplication                                            |
| `Union`/`Intersect`/`Except` | Set operations                             |
| `RowidLookup`| Point lookup via rowid B+tree                            |
| `Insert`     | Source → table (with affinity coercion, defaults, conflict resolution)|
| `Update`     | Source → rowids → re-insert                              |
| `Delete`     | Source → rowids → delete                                 |

**Expression evaluator**: `evaluate(expr, ctx)` walks an `Expr` and produces a `Value`. The context provides:
- The current row (for column lookups)
- Column names (including qualified `alias.column` names from joins)
- Bound parameters

**Function library**: scalar functions (`abs`, `length`, `lower`, `upper`, `coalesce`, `case`, etc.) are implemented in `call_scalar`. Date/time functions are minimal stubs.

---

## Public API

```rust
pub struct Database { /* pager + catalog */ }

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database>;
    pub fn open_in_memory() -> Result<Database>;
    pub fn execute<P: Params>(&mut self, sql: &str, params: P) -> Result<()>;
    pub fn query<P: Params>(&mut self, sql: &str, params: P) -> Result<Vec<Row>>;
    pub fn query_with_columns<P: Params>(&mut self, sql: &str, params: P) -> Result<(Vec<String>, Vec<Row>)>;
}

pub trait Params {
    type Iter: Iterator<Item = Value>;
    fn into_iter(self) -> Self::Iter;
}

// Implemented for: (), Vec<Value>, [Value; N]
```

The API is rusqlite-like: `execute` for statements that don't return rows, `query` for those that do. Parameters are passed as a `Vec<Value>` or array.

---

## Tooling

### CLI Shell (`rustqlite-cli`)

Reads SQL from stdin, one statement per line (terminated by `;`). Supports dot commands (`.help`, `.tables`, `.schema`, `.mode`, `.quit`) and four output modes (table, JSON, CSV, line).

### HTTP/JSON Server (`rustqlite-server`)

A tiny_http-based server with three endpoints:
- `POST /query` — `{"sql": "...", "params": [...]}` → `{"columns": [...], "rows": [...]}`
- `POST /execute` — `{"sql": "...", "params": [...]}` → `{"ok": true}` or `{"error": "..."}`
- `GET /health` → `{"status": "ok"}`

The server is single-threaded (one connection at a time). A `Mutex<Database>` protects against concurrent access.

---

## Trade-offs

### What we did well

- **Layered architecture**: Each layer has a clean contract. You can swap the executor without touching the storage layer.
- **Type safety**: Rust's type system catches most lifecycle bugs at compile time. The `Error` enum centralizes error handling.
- **B+tree correctness**: Tested with randomized inserts, point lookups, range scans, and deletes. The split logic is the trickiest part and is well-tested.
- **WAL integrity**: CRC32 + salt + running checksum makes torn-write recovery robust.
- **SQL coverage**: The parser handles a large subset of SQLite's SQL grammar, including window functions, CTEs, UPSERT, RETURNING.

### What we cut

- **Streaming executor**: Collect-all is simpler but limits memory efficiency. The fix is to move to `Rc<RefCell<>>` for shared state and implement a Volcano-style pull iterator.
- **Index-based query planning**: Indexes are stored but not consulted by the planner. Adding this is mostly mechanical: in the planner, when a `WHERE col = ?` predicate matches an index's first column, replace `Scan` with `IndexScan` + `RowidLookup`.
- **Subquery execution**: Scalar subqueries and `EXISTS` parse but return `Unsupported` at execution. The fix is to add a `Subquery` operator that re-enters the executor recursively.
- **Trigger firing**: Triggers are stored in the catalog but never fired. The fix is to hook into `exec_insert`/`exec_update`/`exec_delete` and dispatch to triggers registered on the affected table.
- **Foreign key enforcement**: Parsed but not enforced. The fix is to add post-INSERT/UPDATE/DELETE checks against referenced tables.

### Performance characteristics

- **Range scans are faster than SQLite** because our executor has less per-row overhead (no VDBE interpretation, no type coercion layer).
- **Point lookups are slower** because we don't yet use the rowid B+tree for `WHERE rowid = ?` — we always scan. Adding `RowidLookup` to the planner (it's already implemented as an operator) would close most of this gap.
- **Inserts are slower** because we flush after every statement. Wrapping in a transaction (or batching flushes) would close the gap.
- **Joins are slower** because we use nested-loop with materialization. A hash join or streaming nested-loop would close the gap.

---

## File Layout

```
src/
├── lib.rs              # Crate root, re-exports
├── error.rs            # Error enum + Result alias
├── api.rs              # Database, Params, execute/query
├── types/
│   ├── mod.rs
│   └── value.rs        # Value, Affinity, Row
├── storage/
│   ├── mod.rs
│   ├── page.rs         # Page, PageType, FileHeader
│   ├── pager.rs        # Pager (LRU cache, freelist)
│   ├── btree.rs        # Btree, Cell, varint
│   ├── wal.rs          # Wal, WalHeader, FrameHeader
│   ├── mvcc.rs         # Snapshot, VersionTracker
│   └── row_codec.rs    # encode_row, decode_row
├── sql/
│   ├── mod.rs
│   ├── lexer.rs        # Lexer, Token, SpannedToken
│   ├── ast.rs          # Statement, Expr, ...
│   └── parser.rs       # Parser
├── schema/
│   └── mod.rs          # Catalog, Table, Index, View, Trigger
├── planner/
│   ├── mod.rs          # Planner, expr_has_aggregate, rewrite_aggregates
│   └── plan.rs         # Plan, ProjectExpr, AggExpr, WindowExpr
├── executor/
│   ├── mod.rs          # execute, operators, ExecContext
│   └── expr.rs         # evaluate, EvalContext, call_scalar
└── bin/
    ├── cli.rs          # rustqlite-cli
    └── server.rs       # rustqlite-server
benches/
├── point_lookup.rs
├── range_scan.rs
├── insert.rs
└── join.rs
examples/
├── basic.rs
├── transaction.rs
└── batch.rs
```

---

## Future Work

The highest-impact improvements, in rough priority order:

1. **Streaming executor**: Move from collect-all to pull-based iterators using `Rc<RefCell<>>` for shared state. This unblocks large result sets and reduces peak memory.
2. **Index-based planning**: Wire `RowidLookup` and `IndexScan` into the planner. Closes the point-lookup gap.
3. **Subquery execution**: Implement scalar subqueries, `IN (subquery)`, `EXISTS (subquery)`. Add a `Subquery` operator that re-enters the executor.
4. **Insert batching**: Group multiple inserts in a transaction into a single WAL flush. Closes the insert gap.
5. **Hash join**: For equi-joins between medium/large relations, build a hash table on the smaller side. Closes the join gap.
6. **Trigger firing**: Hook into DML operators to dispatch to triggers.
7. **FK enforcement**: Post-DML constraint checks.
8. **View expansion**: In the planner, expand view references to their underlying SELECT.
9. **Predicate pushdown**: Push `WHERE` predicates into `Scan` operators so they can use indexes.
10. **Cost-based optimization**: Track table statistics (row counts, cardinality) and choose join order / index selection based on cost.

Each of these is a self-contained project that fits cleanly into the existing architecture.
