# rustqlite Plugin System

rustqlite supports SQLite-style extensions at four levels:

| Level | SQLite equivalent | rustqlite API |
|---|---|---|
| Scalar functions | `sqlite3_create_function` (xFunc) | `Database::create_function` / `rql_api->create_function` |
| Aggregate functions | `sqlite3_create_function` (xStep + xFinal) | `Database::create_aggregate` / same |
| Collations | `sqlite3_create_collation` | `Database::create_collation` / same |
| Virtual tables | `sqlite3_create_module` | `Database::create_module` / same |
| Page codecs | SEE / ZIPVFS (commercial) | `Database::create_codec` + `PRAGMA codec` |

Two ways to register plugins:

1. **Static (Rust, in-process)** — implement the safe traits in
   [`rustqlite::plugin`] and call `Database::create_*`. The types flow
   through the engine's normal dispatch: registered functions are visible
   to the planner, aggregates participate in GROUP BY, collations apply
   to `ORDER BY`/comparisons.
2. **Dynamic (any language, runtime-loadable)** — compile a shared library
   against `include/rustqlite_ext.h` and load it with
   `Database::load_extension("myplugin.so", None)` (feature `extension`).
   The library exports `rustqlite_extension_init` and registers itself
   through the `rql_api` function table — SQLite's loadable-extension
   model. C, C++, Zig, and Rust are all demonstrated in `plugins/`.

---

## 1. Scalar functions (Rust)

```rust
use rustqlite::{Database, Value, Result};
use rustqlite::plugin::{ScalarFunction, FnCtx, Arity};

struct Rot13;

impl ScalarFunction for Rot13 {
    fn name(&self) -> &str { "rot13" }
    fn arity(&self) -> Arity { Arity::Exact(1) }
    fn deterministic(&self) -> bool { true }   // planner may fold constants
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        let s = args.first().map(|v| v.as_text()).unwrap_or_default();
        Ok(Value::Text(rot(&s).into()))
    }
}

let mut db = Database::open_in_memory()?;
db.create_function(Rot13)?;
let rows = db.query("SELECT rot13('hello')", [])?;   // "uryyb"
```

- Name lookup at call sites is **case-insensitive** (SQL convention).
- `Arity::Exact(n)` mismatches error like SQLite (`wrong number of
  arguments to function F(): N supplied`); `Arity::Variadic` accepts any.
- Built-in names (`abs`, `count`, `json_extract`, …) are rejected —
  engine internals cannot be shadowed.
- `deterministic() = true` is a planner hint (constant folding hook).

## 2. Aggregate functions (Rust)

```rust
use rustqlite::plugin::{AggregateFunction, AggState, AggCtx};

struct Median;
impl AggregateFunction for Median {
    fn name(&self) -> &str { "median" }
    fn init(&self) -> Box<dyn AggState> { Box::new(MedianState::default()) }
}

struct MedianState { vals: Vec<f64> }
impl AggState for MedianState {
    fn step(&mut self, _ctx: &AggCtx, args: &[Value]) -> Result<()> {
        if let Some(v) = args.first() { if !v.is_null() { self.vals.push(v.as_real()); } }
        Ok(())
    }
    fn value(&self) -> Result<Value> { /* sort + pick middle */ }
}

db.create_aggregate(Median)?;
db.query("SELECT median(x) FROM t GROUP BY g", [])?;
```

- One state object per group; `value()` is the finalizer.
- An empty group (no rows, or all-NULL input) finalizes the fresh state —
  SQLite's `xFinal`-without-`xStep` semantics.
- Plugin aggregates can be mixed with built-ins in one SELECT
  (`SELECT count(*), median(x) FROM t`).
- The planner recognizes registered aggregates through the statement's
  plugin scope, so GROUP BY/HAVING planning is unchanged.

## 3. Collations

```rust
use rustqlite::plugin::Collation;

struct Reverse;
impl Collation for Reverse {
    fn name(&self) -> &str { "REVERSE" }
    fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        a.chars().rev().collect::<String>().cmp(&b.chars().rev().collect::<String>())
    }
}

db.create_collation(Reverse)?;
db.query("SELECT * FROM t ORDER BY w COLLATE REVERSE", [])?;
db.query("SELECT 'a' = 'A' COLLATE NOCASE", [])?;   // built-in
```

- Built-ins: `BINARY` (the engine's default total order), `NOCASE`
  (ASCII folding, SQLite-compatible), `RTRIM` (trailing-space-insensitive).
- Collations apply wherever SQLite's do: `ORDER BY … COLLATE`,
  comparison operators with a COLLATE operand (`a < b COLLATE X`).
- Like SQLite, collations affect only TEXT–TEXT pairs; numbers keep the
  engine's numeric total order.

## 4. Virtual tables

```rust
use rustqlite::plugin::vtab::*;

struct SeriesModule;
impl VirtualTableModule for SeriesModule {
    fn name(&self) -> &str { "series" }
    fn caps(&self) -> u32 { ModuleCaps::EPHEMERAL }
    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        let end: i64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(10);
        Ok(Box::new(SeriesTable { end }))
    }
    fn connect(&self, t: &str, a: &[String]) -> Result<Box<dyn VirtualTable>> {
        self.create(t, a)                        // ephemeral: same thing
    }
}

impl VirtualTable for SeriesTable {
    fn columns(&self) -> Vec<(String, String)> {
        vec![("n".into(), "INTEGER".into()), ("label".into(), "TEXT".into())]
    }
    fn best_index(&self, cs: &[VtabConstraint]) -> Result<IndexInfo> {
        let mut info = IndexInfo::full_scan(cs.len());
        for (i, c) in cs.iter().enumerate() {
            if c.column == Some(0) && matches!(c.op, VtabConstraintOp::Eq | VtabConstraintOp::Ge) {
                info.handled[i] = true;          // we filter `n = ?` / `n >= ?` ourselves
            }
        }
        info.idx_num = 1;
        Ok(info)
    }
    fn open(&self) -> Result<Box<dyn VirtualTableCursor>> { /* ... */ }
    // fn update(&mut self, ops) when caps() includes WRITABLE
}
```

```sql
CREATE VIRTUAL TABLE s USING series(5);    -- args: ["5"]
SELECT n FROM s WHERE n >= 3;              -- constraint pushed into xFilter
SELECT count(*) FROM s;                    -- aggregates over vtab scans work
INSERT INTO kvstore (k, v) VALUES ...      -- with a WRITABLE module
```

The callback protocol follows SQLite's `sqlite3_module`:

```
CREATE VIRTUAL TABLE t USING mod(args)
        │ xCreate(args) → columns + instance
        ▼
SELECT ... WHERE x = 5
        │ best_index(constraints) → strategy + handled flags
        ▼
open() → filter(idx_num, idx_str, bound_values)
  loop: eof? → column(i) / rowid() → next()
        ▼
INSERT/UPDATE/DELETE → update(ops)     [ModuleCaps::WRITABLE]
```

Semantics worth knowing:

- **Constraint pushdown**: WHERE conjuncts of the shape
  `vtab_col <op> constant/param` are offered to `best_index`; conjuncts
  marked `handled` are passed to `xFilter` as bound values and are NOT
  re-applied by the engine. Everything else becomes a residual filter the
  engine applies itself.
- **Persistence**: `CREATE VIRTUAL TABLE` writes a schema row like any
  table. On reopen the instance is *pending* — it connects on first use
  after you register the module (`db.create_module(...)`), matching
  SQLite's runtime-module linkage. Queries over a pending vtab fail with
  `no such module: <name>`.
- **`xCreate` vs `xConnect`**: create runs for `CREATE VIRTUAL TABLE`;
  connect runs on reopen. Ephemeral modules return the same thing from
  both (see `ModuleCaps::EPHEMERAL`).
- **`DROP TABLE`** calls the module's `destroy`.
- **Streaming**: `SELECT * FROM vtab` streams through the statement API —
  one cursor, rows delivered in batches, never materialized.
- vtabs join against regular tables, participate in aggregates, and
  respect the fast paths (COUNT over a vtab counts cursor rows, not
  B+tree cells).

## 5. Page codecs

Transform every page between its in-memory and on-disk form — the hook
SQLite's SEE encryption and ZIPVFS compression use.

```rust
use rustqlite::plugin::codec::XorCodec;

let mut db = Database::open("secret.db")?;
db.create_codec(XorCodec::new(0x5A))?;
db.execute("PRAGMA codec = xor", [])?;      // activate
```

- Both `encode` and `decode` receive/return exactly page-size bytes
  (fixed positional layout — compress-then-pad).
- Page 0's first 100 bytes (file header + codec marker) stay plain, so
  the file remains recognizable.
- The codec name is recorded in the header: a plain `Database::open` of
  a coded file fails with a pointer to `Database::open_with_codec`,
  and a mismatched codec is refused.
- WAL mode is disabled while a codec is active (journal frames are not
  encoded) — `PRAGMA journal_mode=WAL` errors.
- `PRAGMA codec` (read form) reports the active/required codec.

## 6. Dynamic extensions (C / C++ / Zig / Rust)

Build against `include/rustqlite_ext.h` and export:

```c
int rustqlite_extension_init(const rql_api *api, rql_db *db, char **err);
```

```c
static void rot13_func(rql_context *ctx, int argc, rql_value **argv) {
    int len = 0;
    const unsigned char *txt = rql->value_text(argv[0], &len);
    /* ... transform ... */
    rql->result_text(ctx, buf, len);
}

int rustqlite_extension_init(const rql_api *api, rql_db *db, char **err) {
    rql = api;
    api->create_function(db, "rot13", 1, 0, NULL, rot13_func, NULL, NULL);
    api->create_module(db, "series", &series_module, NULL);
    return RQL_OK;
}
```

Load:

```rust
let mut db = Database::open("app.db")?;
db.load_extension("rot13.so", None)?;      // feature "extension" (default on)
```

- Status/type codes match SQLite (`RQL_OK=0`, `RQL_ROW=100`,
  `RQL_DONE=101`, `RQL_INTEGER=1`, …).
- Results are set through the context (`result_int64`, `result_text`,
  `result_error`, …) — SQLite's `sqlite3_result_*` model, including
  `aggregate_context` for xStep/xFinal state.
- The API table is process-lifetime — extensions may keep the pointer.
- Building: `tests/build_plugins.sh` compiles all four examples.

Examples in `plugins/`:

| Path | Language | Registers |
|---|---|---|
| `plugins/c/rot13.c` | C | `rot13` function, `sumsq` aggregate, `ROT13` collation, `series` vtab |
| `plugins/cpp/example.cpp` | C++ | `shout` function, `movavg` aggregate (POD ring buffer), `NUMERIC` collation, writable `kvstore` vtab |
| `plugins/zig/rot13.zig` | Zig | `rot13`, `zcount` aggregate, `ZREVERSE` collation, `zrange` vtab |
| `plugins/rust/` | Rust cdylib | `revsum` variadic function, `product` aggregate, `mirror` vtab — zero engine linkage, ABI only |

**C++ gotcha**: `aggregate_context` returns zeroed RAW memory. A C++
aggregate state with non-trivial members (`std::deque`, `std::string`)
must placement-new on first use and destroy in xFinal — or keep the state
a POD, like the examples do.

## 7. SQLite-style C API (`rustqlite_*`)

`src/ffi.rs` exposes the engine through a SQLite-shaped C API for
drivers (this is the layer a future sqlx backend binds to):

```c
rustqlite_open / rustqlite_open_in_memory / rustqlite_close
rustqlite_exec
rustqlite_prepare_v2 / rustqlite_step / rustqlite_finalize / rustqlite_reset
rustqlite_bind_int64 / _double / _text / _blob / _null
rustqlite_column_count / _name / _type / _int64 / _double / _text / _blob / _bytes
rustqlite_changes / rustqlite_total_changes / rustqlite_last_insert_rowid
rustqlite_errcode / rustqlite_errmsg / rustqlite_libversion
rustqlite_create_function / rustqlite_create_collation / rustqlite_create_module
rustqlite_load_extension
```

Semantics carried over from SQLite: 1-based binds, 0-based columns,
RQL_ROW/RQL_DONE stepping, `close` refuses while statements are live
(SQLITE_BUSY), text pointers valid until the next step, prepared
statements re-executed with `reset`, read steps share the connection
read-lock (concurrent readers), writes serialize.

## 8. Prepared statements (Rust)

The `sqlite3_prepare/step` model natively:

```rust
let mut stmt = db.prepare("SELECT id, x FROM t WHERE id > ? ORDER BY id")?;
stmt.bind(1, Value::Integer(50));            // 1-based like SQLite
while stmt.step()? == StepResult::Row {
    println!("{} {}", stmt.column_int(0), stmt.column_text(1).unwrap());
}
stmt.reset();                                 // re-run with current bindings
```

`SELECT`/DML plans with the shapes `Scan`, `RowidRange`, `Filter`,
`Project`, `Limit` (and virtual-table scans) **stream**: resumable
drivers pull rows in batches of 64 with early termination — a 1M-row scan
never materializes. Aggregates, joins, sorts and CTEs execute once and
serve from the materialized result (still no re-parse / re-plan on
rebind). DDL and transaction control go through `Database::execute`.

## Dispatch model (performance)

The registry lives on `Database` behind an `Arc` snapshot. Statements
install the snapshot into a thread-local scope — the same pattern as the
engine's correlated-subquery bridge — so deeply nested evaluator code
reaches it without threading a parameter through every signature.
Costs:

- Zero plugins registered: one relaxed atomic load per statement
  (the `has_plugins` fast path).
- With plugins: one RwLock read + Arc clone + thread-local install per
  statement; dispatch is a HashMap probe on the unknown-name fallback
  AFTER the built-in match, so plugin-less workloads never slow down.

Statement caches are invalidated on plugin registration (plans may hold
`Arc<Table>` schemas affected by vtab connects).
