# Gap Analysis — Where rustqlite Still Loses to SQLite

> Baseline measured `2026-08-28` after commit `a228800` on a clean `cargo build --release` + `cargo test --release` (59 tests / 304 internal cases, all passing).
> Workload: `cargo run --release --example bench_compare`.

## 1. Current performance gap (lower ratio = closer to SQLite)

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | 2-table join, filter by PK, ~10 rows out | 8.90 ms | 37.10 µs | **240×** slower | 🔴 biggest gap |
| 2 | 3-table join, filter by PK, ~50 rows out | 22.13 ms | 124.85 µs | **177×** slower | 🔴 |
| 3 | UPDATE range (`val > 5000`) | 29.62 ms | 1.33 ms | **22×** slower | 🔴 |
| 4 | Range scan (10 rows) | 35.53 µs | 3.80 µs | **9.4×** slower | 🟡 per-query overhead |
| 5 | Point lookup by rowid (1k ops) | 3.18 ms | 348 µs | **9.1×** slower | 🟡 per-query overhead |
| 6 | Mixed 80/20 (5k ops) | 20.38 ms | 2.45 ms | **8.3×** slower | 🟡 composite |
| 7 | Full scan + COUNT with filter | 3.19 ms | 539 µs | **5.9×** slower | 🟡 vectorization gap |
| 8 | Aggregate (SUM/AVG/MIN/MAX) | 7.18 ms | 1.24 ms | **5.8×** slower | 🟡 vectorization gap |
| 9 | UPDATE by PK (1k ops) | 10.92 ms | 1.84 ms | **5.9×** slower | 🟡 per-row overhead |
| 10 | Single-row inserts (100k, in txn) | 533 ms | 129 ms | **4.1×** slower | 🟡 per-row overhead |
| 11 | DELETE by PK (1k ops) | 5.22 ms | 1.37 ms | **3.8×** slower | 🟡 |
| 11 | Single-row inserts (auto-commit) | 5.67 ms | 1.87 ms | **3.0×** slower | 🟡 no auto-txn |
| 12 | Multi-row VALUES batches (10k) | 23.61 ms | 6.58 ms | **3.6×** slower | 🟢 already optimized |
| 13 | DB file size (10k rows) | 917.50 KB | 262.14 KB | **3.5×** larger | 🟡 no free-page reuse |
| 14 | Range scan (100 rows) | 37.16 µs | 11.51 µs | **3.2×** slower | 🟢 |
| 15 | 2-table join + GROUP BY | 12.42 ms | 3.02 ms | **4.1×** slower | 🟡 |
| 16 | Range scan (5000 rows) | 1.23 ms | 568 µs | **2.2×** slower | 🟢 |
| 17 | GROUP BY (100 buckets) | 4.32 ms | 1.94 ms | **2.2×** slower | 🟢 |
| 18 | Range scan (1000 rows) | 211 µs | 107 µs | **2.0×** slower | 🟢 |
| 19 | Point lookup by indexed col (1k ops) | 927 µs | 532 µs | **1.7×** slower | 🟢 nearly at parity |
| 20 | Peak RSS (100k insert) | 44.6 MB | 25.4 MB | **1.75×** larger | 🟢 |
| 21 | Binary size (stripped) | 1.01 MB | 1.98 MB | **0.51×** — we win | ✅ |

## 2. Where we still lose — categorized by fix area

### 2.1 JOIN — biggest absolute gap (240× and 177×)

**Symptom**: `SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?`
with `u.id = 500` produces ~10 rows but takes 8.9 ms (vs SQLite 37 µs).

**Root cause**: The hash join **fully materializes the right side first**:
1. `execute(left, ctx)` — Filter { Scan users, u.id = ? } → 1 row ✅ fast
2. `execute(right, ctx)` — Scan orders → **decodes all 10 000 rows** ← bottleneck
3. Build hash on the 10 000 right rows
4. Probe with 1 left row → ~10 matches

Even though only ~10 right rows will ever match, we pay the cost of decoding
all 10 000 of them. This is the canonical case for **index nested-loop join**:
for each outer row, look up matching inner rows via the inner table's index.

**Fix**: `Plan::IndexNestedLoopJoin { outer, inner_table, inner_index, outer_key_expr, inner_key_expr }`
- For each outer row, encode `outer_key_expr` against the outer row, then call
  `Btree::lookup_index` on the inner index, then look up each matching rowid.
- When the inner table has no index on the join key, fall back to hash join.

**Sub-fix** (predicate pushdown for equi-join keys): when the WHERE clause
contains `left.k = const` and the join condition is `left.k = right.k`,
transitively push `right.k = const` into the right-side scan. This converts
the right side from `Scan` to `IndexLookup` if there is an index on `right.k`.

### 2.2 UPDATE with WHERE on non-PK column — 22× gap

**Symptom**: `UPDATE t SET score = score + 1.0 WHERE val > 5000` (10k rows, `val` indexed).

**Root cause**: `exec_update` calls `execute(source, ctx)` where `source` is
planned as a full `Scan + Filter`, not as `IndexScan(val > 5000)`. So we scan
all 10k rows, decode each, filter, then update by rowid.

**Fix**: planner picks `Plan::IndexRange` for `WHERE indexed_col > ?` (mirror
the existing `RowidRange` plan node for `WHERE rowid > ?`). The update path
then receives only matching rowids directly.

### 2.3 Per-query overhead — 9× on point lookup, 9.4× on small range scan

**Symptom**: 1 000 rowid lookups take 3.18 ms = ~3.2 µs/op. SQLite does it in
0.35 µs/op. Even with all parsing/caching eliminated, the floor is ~3 µs.

**Suspected hot-path costs** (per call):
1. `Database::query` → `Database::get_or_cache_stmt` — `HashMap<String, _>::get`
   with a String hash + comparison. ~100 ns.
2. `ExecContext::new` — clones `params: HashMap<String, Value>` into a new
   map per call (no — actually `params: HashMap<String, Value>` is moved in,
   but `bind_params` allocates a new HashMap each call). ~150 ns + alloc.
3. `Btree::new(ctx.pager, root, false)` — allocates a fresh `Btree` struct
   per call (cheap), but goes through `Rc::clone` on pager pages.
4. `Btree::lookup_table(rowid)` — root-to-leaf traversal with `Rc<RefCell<Page>>`
   indirection. Each page fetch goes through `Pager::fetch_page` →
   `Rc::clone` of the cached page, then `RefCell::borrow()`.
5. `decode_row(payload, n_cols)` — parses each value out of the byte buffer.

**Fixes** (in order of impact):
- **Inline decode** for single-row lookups: skip building a `Vec<Row>` and
  return the row directly from `exec_rowid_lookup`.
- **Pre-compute catalog lookups** in the `Plan` itself — store `Arc<Table>`
  in the plan so the executor doesn't dereference the catalog per call.
- **Avoid `HashMap` for params when there's only 1** — fast path for the
  common case (one `?` parameter).
- **Profile-guided inlining** of the `Btree::lookup_table` hot path.

### 2.4 Single-row inserts in auto-commit — 3× gap

**Symptom**: 1 000 single-row INSERTs outside a transaction take 5.67 ms vs
SQLite's 1.87 ms.

**Root cause**: each INSERT statement:
1. Opens a `BEGIN` implicitly.
2. Writes the row.
3. Commits (flushes WAL).

SQLite batches consecutive auto-commit INSERTs transparently when
`synchronous=NORMAL + journal_mode=WAL` — we don't.

**Fix**: `Database::execute` should detect a burst of INSERTs (same table,
short interval) and transparently wrap them in BEGIN/COMMIT, flushing only
on idle. Mirrors SQLite's "transaction group" optimization.

### 2.5 Single-row inserts in explicit transaction — 4.1× gap (100k rows)

**Symptom**: 100 000 INSERTs in `BEGIN/COMMIT` take 533 ms (5.3 µs/row) vs
SQLite's 129 ms (1.3 µs/row).

**Root cause**: per-row overhead in `exec_insert`:
1. `get_or_scan_max_rowid` — cached after the first call, so this is just a
   `HashMap::get`. ~50 ns.
2. `Btree::lookup_table` to check for rowid collisions on `OR REPLACE` paths.
   The "auto-generated rowid" fast path skips this; the slow path doesn't.
3. `Btree::insert_table` — root-to-leaf traversal + leaf insert + possible split.
4. Per-statement `ctx.pager` borrow — this is per-statement, not per-row.

**Fix**: **cursor-based insert** — for batched inserts into the same table,
retain the Btree leaf cursor between inserts so we skip root-to-leaf traversal
on sequential rowids (the common case). This is the same trick SQLite uses
in its `Insert` opcode.

### 2.6 Aggregate / full scan — 5.8× / 5.9× gap

**Symptom**: `SELECT SUM(score), AVG(score), MIN(score), MAX(score) FROM t`
on 10 000 rows takes 7.18 ms vs SQLite's 1.24 ms.

**Root cause**: row-at-a-time evaluation:
1. `decode_row` per row — parses each value out of the byte buffer.
2. `evaluate` per row — walks the AST per row.
3. `Vec<Row>` allocation per row — even when the aggregate only needs one
   column.

**Fix**: **vectorized scan** — for `SELECT agg(col) FROM t WHERE pred`:
- Decode only the columns the aggregate references.
- Evaluate the predicate in a tight loop over a column batch.
- Update aggregate accumulators in the same loop.
- Never materialize a `Vec<Row>`.

### 2.7 GROUP BY — 2.2× gap

**Symptom**: `SELECT dept, COUNT(*) FROM users GROUP BY dept` (100 buckets)
takes 4.32 ms vs SQLite's 1.94 ms.

**Root cause**: same as 2.6, plus `HashMap<Vec<Value>, AggState>` overhead
per group. The key is `Vec<Value>` which allocates per row.

**Fix**: encode the group key as `Vec<u8>` (like the hash join already does),
and use that as the HashMap key. Eliminates one `Vec<Value>` allocation per
input row.

### 2.8 File size — 3.5× larger

**Symptom**: 10k rows on disk → 917 KB vs SQLite's 262 KB.

**Root cause**: no free-page reuse list. When rows are deleted, the pages
become orphaned but aren't reclaimed. Also, our row codec may use more bytes
per value than SQLite's varint encoding.

**Fix**: implement a free-page list in page 0 (SQLite uses page 1). On
INSERT, consult the free list first. On DELETE, append the freed page.

### 2.9 UPDATE by PK — 5.9× gap

**Symptom**: 1 000 `UPDATE t SET score = ? WHERE id = ?` take 10.92 ms
(10.9 µs/op) vs SQLite's 1.84 ms (1.8 µs/op).

**Root cause**: per-update overhead in `exec_update`:
1. Plan source: `RowidLookup` — good, uses the index. ~1 µs.
2. `Btree::lookup_table` to read the row → ~1 µs.
3. `decode_row` to get old values → ~0.5 µs.
4. Apply SET expressions → ~0.5 µs.
5. `Btree::delete_table` + `Btree::insert_table` → ~3 µs each.
6. WAL append per statement → ~3 µs.

**Fix**: in-place update when the row size doesn't change (the common case
for `UPDATE t SET x = ?` where `x` is the same length). Skip the delete +
insert dance; just overwrite the leaf cell payload.

### 2.10 DELETE by PK — 3.8× gap

**Symptom**: 1 000 `DELETE FROM t WHERE id = ?` take 5.22 ms (5.2 µs/op) vs
SQLite's 1.37 ms (1.4 µs/op).

**Root cause**: same shape as 2.9 — `Btree::delete_table` per row, plus
per-statement WAL append.

**Fix**: same as 2.9 (batch WAL appends). Also: when the leaf page becomes
empty, return it to the free-page list (ties into 2.8).

### 2.11 Mixed 80/20 — 8.3× gap

**Symptom**: 5 000 ops (80% reads / 20% writes) take 20.38 ms vs SQLite's
2.45 ms.

**Root cause**: composite — point lookups (2.3) × 4 000 + inserts (2.4) ×
1 000 + per-op overhead.

**Fix**: fix 2.3 + 2.4 and this falls out automatically.

### 2.12 Range scan (10 rows) — 9.4× gap

**Symptom**: `SELECT * FROM t WHERE id BETWEEN 1 AND 10` takes 35 µs vs
SQLite's 3.8 µs.

**Root cause**: per-query overhead dominates at this scale (10 rows ×
~3 µs/row decode + 30 µs fixed overhead).

**Fix**: same as 2.3 (lower per-query overhead) + skip the `Vec<Row>`
allocation when the result is small (return a small-box-optimized array).

---

## 3. Functional gaps (features SQLite has, we don't)

### 3.1 SQL surface

- [ ] **Foreign keys** — `FOREIGN KEY ... REFERENCES ...`, `ON DELETE CASCADE/SET NULL/RESTRICT/SET DEFAULT`, `ON UPDATE ...`. Gated by `PRAGMA foreign_keys = ON`.
- [ ] **Triggers** — `CREATE TRIGGER ... BEFORE/AFTER INSERT/UPDATE/DELETE ON ... FOR EACH ROW WHEN ... BEGIN ... END`. Includes `OLD`/`NEW` row refs and `UPDATE OF column-list` filtering.
- [ ] **Views** — `CREATE VIEW ... AS SELECT ...`, view resolution in planner.
- [ ] **CTEs** — `WITH ... AS (...) SELECT ...` (parsed, not executed) and `WITH RECURSIVE` (needs fixpoint loop).
- [ ] **Window functions** — plan node exists but `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTILE` return `Value::Null`.
- [ ] **Prepared statements** — `?` and `:name` binding exists but no `EXPLAIN`.
- [ ] **PRAGMA surface** — only a stub; need `foreign_keys`, `journal_mode`, `synchronous`, `cache_size`, `table_info`, `index_list`, `wal_checkpoint`, `integrity_check`, `optimize`.
- [ ] **CHECK constraints** — `CHECK (expr)` parsed but not enforced on INSERT/UPDATE.
- [ ] **DEFAULT expressions** — `DEFAULT CURRENT_TIMESTAMP`, `DEFAULT (random())` not supported.
- [ ] **Generated columns** — `GENERATED ALWAYS AS (expr) STORED/VIRTUAL`.
- [ ] **ALTER TABLE** — `RENAME TABLE`, `RENAME COLUMN`, `ADD COLUMN`, `DROP COLUMN` not supported.
- [ ] **UPSERT** — `INSERT ... ON CONFLICT (col) DO UPDATE SET ... DO NOTHING` not supported (OR REPLACE / OR IGNORE exist).
- [ ] **RETURNING** — `INSERT/UPDATE/DELETE ... RETURNING ...` not supported.
- [ ] **SQL functions** — missing `COALESCE`, `IFNULL`, `NULLIF`, `LENGTH`, `LOWER`, `UPPER`, `SUBSTR`, `TRIM`, `LTRIM`, `RTRIM`, `REPLACE`, `ROUND`, `ABS`, `RANDOM`, `DATE`, `TIME`, `DATETIME`, `STRFTIME`, `JULIANDAY`, `PRINTF`, `INSTR`, `HEX`, `UNHEX`, `TYPEOF`, `QUOTE`, `GLOB`, `LIKE` (with escape), `MIN/MAX` (scalar form).
- [ ] **SAVEPOINTs** — `SAVEPOINT name`, `RELEASE name`, `ROLLBACK TO name`.

### 3.2 Storage & recovery

- [ ] **On-disk format compatibility** — magic is `"RSQLDB01"`, SQLite is `"SQLite format 3\0"`. Need either byte-compat or `import_sqlite`/`export_sqlite` shims.
- [ ] **Crash-safe WAL** — per-frame CRC32 + salt + running checksum exists; need to verify recovery replays correctly after simulated crash mid-commit.
- [ ] **Checkpoint strategies** — `PRAGMA wal_checkpoint(PASSIVE | FULL | RESTART | TRUNCATE)`. Only one mode currently.
- [ ] **Online backup** — `Database::backup(target)` API mirroring `sqlite3_backup_*`.
- [ ] **VACUUM** — full + incremental; rebuild file to reclaim free pages.
- [ ] **Free-page list** — page 0 should track freed pages for reuse on INSERT.
- [ ] **Partial index** — `CREATE INDEX ... WHERE ...`.
- [ ] **Covering index** — `INCLUDE` columns in the index payload.
- [ ] **WITHOUT ROWID tables** — `CREATE TABLE ... WITHOUT ROWID` uses PK as cluster key.

### 3.3 Concurrency & MVCC

- [ ] **MVCC visibility wired in** — `Snapshot` / `VersionTracker` in `src/storage/mvcc.rs` exist but the query path doesn't call them. Readers should consult the WAL up to a stable frame count for snapshot isolation.
- [ ] **`Arc<RwLock<Database>>` server** — currently `Mutex<Database>` serializes all reads.
- [ ] **`Rc<RefCell<Page>>` → `Arc<Mutex<Page>>`** in the pager cache — blocks `Database: Send + Sync`.
- [ ] **Transaction isolation levels** — BEGIN/DEFERRED/IMMEDIATE/EXCLUSIVE not enforced.
- [ ] **Connection pool** with WAL-mode isolation — N read connections + 1 write.
- [ ] **Deadlock detection** (if we ever ship row-level locks).
- [ ] **Stress test** — 100 concurrent connections, mixed read/write, assert no corruption.

### 3.4 Productionization

- [ ] **Error type overhaul** — `Box<dyn Error>` → `thiserror` enum with SQL text + line + span.
- [ ] **Structured logging** via `tracing` — configurable per-module, JSON output.
- [ ] **Metrics** — query latency p50/p95/p99, cache hit rate, WAL size, tps, active connections.
- [ ] **CLI improvements** — `.tables`, `.schema`, `.explain`, `.timer`, `.mode csv/json/table`, `.import`, `.dump`, `.restore`, `.read`.
- [ ] **Driver ecosystem** — `sqlx` adapter crate so Rust apps can swap `rusqlite` for `rustqlite`. Stretch: SeaORM, Diesel.
- [ ] **Docs** — mdbook user guide, embedding guide, migration guide, perf tuning guide.
- [ ] **`cargo-fuzz` targets** — parser, storage, SQL semantics fuzzers with differential checking.
- [ ] **CI** — GitHub Actions workflow running tests + bench_compare; upload benchmark as artifact.
- [ ] **Coverage gate** — `cargo tarpaulin` targeting 70% line coverage on `src/`.
- [ ] **Benchmarks dashboard** — commit `benches/baseline.json`; CI renders regression chart.
- [ ] **Release automation** — `cargo release` config, CHANGELOG.md, GitHub Releases with built binaries.

### 3.5 Test parity (SQLite has, we don't)

- [ ] **Differential fuzzer** `tests/fuzz_differential.rs` — random SQL generator running against `rusqlite` (oracle) and `rustqlite`, asserting value-by-value equality. Currently 164 hand-written cases; goal 500+.
- [ ] **SLT corpus** — only `select1.test`, `select2.test`, `select3.test` (140 cases). Need `index/btree/boundary{1,2,3}.test`, `e_createtbl.test`, `e_insert.test`, `e_select.test`.
- [ ] **Crash recovery tests** — write WAL frames, simulate crash mid-checkpoint, reopen, verify committed data intact.
- [ ] **Migration tests** — open v0.1 database file with the next release; ensure backward compat.
- [ ] **Property-based tests** via `proptest` — B+tree invariants, row codec round-trip, SQL parser round-trip.
- [ ] **TPC-C scaffold** — 5-table, 9-transaction concurrency + correctness test.
- [ ] **TPC-H subset** — Q1, Q3, Q5, Q10 as analytical workload tests.

### 3.6 Stretch goals (where we can pull ahead)

- [ ] **Columnar storage mode** — for analytical tables, store column-major.
- [ ] **Vectorized execution** — column-batch-at-a-time WHERE/projection/aggregate.
- [ ] **`io_uring` async I/O** — on Linux, batch page reads for cold-cache scans.
- [ ] **Multi-threaded parallel scan** — split B+tree leaf range across N threads, aggregate partial results.
- [ ] **Per-page LZ4/zstd compression** — hot/cold data separation.
- [ ] **Materialized views** — auto-maintain a materialized view on writes.
- [ ] **Query result cache** — for read-heavy workloads with repeated queries.
- [ ] **JIT codegen** via Cranelift — for hot queries, skip interpreter dispatch.

---

## 4. Working order (this sprint)

1. ✅ Verify baseline (cargo build/test clean, bench_compare runs).
2. ✅ Write this `GAP_ANALYSIS.md`.
3. ⏳ **IndexNestedLoopJoin** — closes 2-table join 240× gap + 3-table join 177× gap.
4. ⏳ **UPDATE/DELETE via IndexScan** — closes UPDATE range 22× gap.
5. ⏳ **Per-query overhead reduction** — closes point lookup 9.1× + small range scan 9.4×.
6. ⏳ **Auto-txn for consecutive INSERTs** — closes auto-commit inserts 3× gap.
7. ⏳ **Cursor-based batch insert** — closes in-txn inserts 4.1× gap.
8. ⏳ **Vectorized scan + aggregate** — closes aggregate 5.8× + full scan 5.9× gaps.
9. ⏳ **In-place UPDATE** — closes UPDATE by PK 5.9× gap.
10. ⏳ **Free-page list + VACUUM** — closes file size 3.5× gap.
11. ⏳ **MVCC visibility wired into query path** + `Arc<RwLock<Database>>` server — unblocks concurrency parity.
12. ⏳ Push to `iamleson98/rust-sql`, update `worklog.md`.

---

## 5. Tracking conventions

- Tick a box when the work is **merged to master and CI is green**.
- For perf items, re-run `bench_compare` and update the table at the top of this file.
- For test items, add a row to `TESTS.md` with case count + pass rate.
- Every commit should be small and self-contained; prefer many small commits over a giant "wip" push.
- After every perf-related commit, re-run `bench_compare` and update `PRODUCTION_TODO.md` baseline.
