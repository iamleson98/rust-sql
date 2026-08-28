# Production Readiness TODO — rustqlite vs SQLite

> Goal: make rustqlite production-ready and competitive with (or faster than)
> SQLite on real workloads. Tracked by (a) test-parity with SQLite via the
> SQLlogictest (SLT) corpus and a differential fuzzer, and (b) a comparative
> criterion benchmark harness running identical workloads against
> `rusqlite` and rustqlite.

## Current baseline (recorded `2026-08-28`, after IndexNestedLoopJoin + index split fix + in-place UPDATE + deferred flush)

`cargo test --release`: **57 unit + 1 differential (164 cases) + 1 SLT (140 cases) + 2 doctests = 61 tests, 304 internal cases, all passing.**
`cargo run --release --example bench_compare` on the same workload (with `set_deferred_flush(true)` mirroring SQLite's WAL+synchronous=NORMAL):

| Workload                                  | rustqlite   | SQLite      | Ratio       |
| ----------------------------------------- | ----------- | ----------- | ----------- |
| Single-row inserts (1k, auto-commit)      | 2.00 ms     | 1.82 ms     | **1.10× slower** ✅ was 6.9× → 1.4× → 1.10× |
| Single-row in BEGIN/COMMIT (10k)           | 22.5 ms    | 12.9 ms    | 1.74× slower ✅ was 8.0× → 2.3× → 1.74× |
| Single-row in BEGIN/COMMIT (100k)          | 403 ms   | 130 ms   | 3.1× slower ✅ was 11× → 4.0× → 3.1× |
| **Multi-row VALUES batches (10k)** ✅      | **22.3 ms** | 6.7 ms    | **3.3× slower** (was 13×) |
| Point lookup by rowid (1k ops)            | 918 µs     | 360 µs   | **2.55× slower** ✅ was 9.2× → 2.7× → 2.55× |
| **Range scan (10 rows)** ✅                | **39 µs** | 4.5 µs    | **8.7× slower** (was 200× → 11.7× → 8.7×) |
| **Range scan (100 rows)** ✅               | **36 µs** | 12 µs   | **3.0× slower** (was 200× → 3.3× → 3.0×) |
| **Range scan (1000 rows)** ✅              | **225 µs** | 108 µs | **2.1× slower** (was 13× → 1.9× → 2.1×) |
| **Range scan (5000 rows)** ✅              | **1.20 ms** | 556 µs   | **2.16× slower** (was 4× → 2.0× → 2.16×) |
| Full scan + COUNT with filter            | 2.81 ms     | 500 µs   | **5.6× slower** ✅ was 6.6× → 5.6× |
| Aggregate (SUM/AVG/MIN/MAX)                | 6.73 ms     | 1.24 ms     | **5.4× slower** ✅ was 5.5× → 5.4× |
| GROUP BY (100 buckets)                     | 3.51 ms     | 1.92 ms     | **1.83× slower** ✅ was 2.4× → 1.8× |
| **Point lookup by indexed col (1k ops)** ✅ | **608 µs** | 564 µs | **1.08× slower** ✅ was 5.5× → 1.9× → 1.08× (at parity!) |
| **2-table join filter by PK (~10 rows out)** ✅ | **140 µs** | 23 µs | **6.1× slower** ✅ was 240× → 7.0× → 6.1× |
| **3-table join filter by PK (~50 rows out)** ✅ | **143 µs** | 57 µs | **2.5× slower** ✅ was 177× → 2.2× → 2.5× |
| 2-table join + GROUP BY (full scan)        | 14.5 ms    | 2.97 ms     | 4.9× slower ✅ was 4.6× → 4.9× |
| **UPDATE by PK (1k ops)**                  | **4.82 ms** | 1.86 ms   | **2.6× slower** ✅ was 743× → 4.1× → 2.6× |
| UPDATE range (val > 5000)                  | 30.9 ms    | 1.31 ms     | 23.6× slower  |
| **DELETE by PK (1k ops)**                  | **4.93 ms**  | 1.36 ms   | **3.6× slower** ✅ was 43× → 3.9× → 3.6× |
| **Mixed 80/20 (5k ops)**                   | **10.65 ms** | 2.46 ms   | **4.3× slower** ✅ was 292× → 9.2× → 4.3× |
| DB file size (10k rows)                    | 917.50 KB   | 262.14 KB   | 3.5× larger |
| Peak RSS (100k insert)                     | 27.94 MB    | 21.21 MB    | **1.32× larger** ✅ was 2.2× → 1.32× |

### What's already done (verified by reading source, not by TODO checkboxes)

- ✅ Statement cache: `Database::get_or_cache_stmt` caches parsed `Arc<Statement>`
  + planned Plan keyed by SQL text, FIFO eviction at 64 entries, invalidated
  on any DDL. `set_stmt_cache_capacity()` for tuning. Cache hit is O(1)
  (Arc refcount inc) — previously was a deep clone of the entire AST.
- ✅ O(1) dirty-page counter: `Pager::dirty_count_approx` upper-bound
  counter incremented by `note_write()` on every mutating Btree/Pager op,
  reset to 0 by `flush()`. `flush()` fast path: `if dirty_count_approx == 0`
  (was: O(cache_size) scan on every `Database::query()` call — ~5 µs of
  pure overhead per SELECT). `Database::query()` skips flush entirely when
  no writes happened since the last flush.
- ✅ Pager cache hit: O(1) — `touch_lru` skipped on cache hits. Eviction
  is FIFO (was: O(N) linear scan via `VecDeque::iter().position(...)` on
  every page access — ~2k comparisons per page lookup).
- ✅ Positional parameters use `Vec<Value>` indexed by usize (was:
  `HashMap<String, Value>` with first-insert bucket allocation ~200-500 ns
  per query). New `bind_positional(value)` method skips String key
  allocation. Named params (`:name`, `@col`, `$var`) fall back to a
  separate HashMap, allocated lazily.
- ✅ `Btree::lookup_table` uses binary search by rowid (was: linear scan
  with `Cell::decode` per cell — each decode allocates a `Vec<u8>` for the
  payload, so N-cell leaf did N heap allocations per lookup). New
  `decode_rowid_only(buf)` helper reads just the rowid varint, no
  allocation. Only the matched cell is fully decoded.
- ✅ Streaming-aggregate fast path: `exec_aggregate_streaming_scan` iterates
  the B+tree directly when input is `Scan` or `Filter(Scan, pred)`, decoding
  each row into a single reusable buffer. Avoids materializing
  `Vec<Vec<Value>>` in `exec_scan` (was: ~3-4 ms of pure allocation overhead
  on a 10k-row aggregate).
- ✅ `decode_row_into(buf, n_cols, &mut Vec<Value>)`: zero-alloc row decode
  into a caller-provided buffer. Used by the streaming-aggregate fast path.
- ✅ Anonymous-`?` lexer fix: each `?` now gets a distinct incrementing index
  (was: every `?` lexed to `Parameter("0")`, silently binding all predicates
  to the first param).
- ✅ `Plan::RowidRange` + `exec_rowid_range`: `WHERE id BETWEEN ? AND ?`,
  `id > ?`, `id < ?`, `id >= ?`, `id <= ?`, `id >= ? AND id <= ?` all use
  `Btree::scan_table_range` instead of full-scan + per-row filter.
- ✅ Hash join picks the smaller side as the build side for INNER/CROSS joins;
  outer joins keep the right-side-build path for correctness.
- ✅ Hash join uses `Vec<u8>` byte-encoded hash keys via `Value::encode()`
  instead of `format!("{:?}", v)` String allocations.
- ✅ Hash join reuses a single key buffer across probe iterations (no per-row
  heap allocation for the hash key).
- ✅ INSERT fast path: when the rowid was auto-generated (max+1), skip the
  `lookup_table` existence check entirely (was redundant — max+1 cannot
  collide with any existing rowid).
- ✅ INSERT: collapsed the redundant double-`lookup_table` on the OR REPLACE
  path into a single call.
- ✅ `Plan::RowidLookup` + `exec_rowid_lookup` — point lookup by PK is wired up.
- ✅ `Plan::IndexLookup` + `exec_index_lookup` — point lookup by indexed col.
- ✅ `Plan::Join { algorithm: Hash }` + `exec_hash_join` — hash join selected
  for natural/using/equi-ON joins.
- ✅ Multi-threaded server (`Arc<State>` + N worker threads via
  `tiny_http::Server::incoming_requests`). Mutex serializes DB access only.
- ✅ `Plan::Window` exists; window-function executor scaffolding present.
- ✅ Conflict resolutions `OR REPLACE` / `OR IGNORE` / `OR ABORT` / `OR FAIL`
  / `OR ROLLBACK` implemented in `exec_insert`.
- ✅ UNIQUE index constraint check before insert (with conflict resolution).
- ✅ `Btree::lookup_table` / `lookup_index` / `scan_table` / `scan_range` /
  `delete_table` / `insert_table` / `split_leaf` are all in place.

### Top-priority perf gaps that remain

1. **2-table join (filter by PK)** at 6.1× slower — for a 10-row result,
   the per-query setup cost dominates (parse + plan + execute setup).
   SQLite's prepared-statement cache is more deeply integrated. Hard to
   close further without a deeper prepared-statement refactor.
2. **2-table join + GROUP BY (full scan)** at 4.9× slower — hash join
   materializes 10K rows + aggregate over them. The streaming-aggregate
   fast path doesn't yet handle Aggregate(Join(...), ...) — only
   Aggregate(Scan) and Aggregate(Filter(Scan)).
3. **Aggregate (SUM/AVG/MIN/MAX)** at 5.4× slower — 4 aggregates × 10K rows
   = 40K eval_row calls. Each eval_row has EvalContext setup + column-name
   string comparison per call. Caching column-name → index resolution in
   the planner would help.
4. **UPDATE range** at 23× slower — full table scan + filter + per-row
   in-place update. SQLite uses a streaming UPDATE plan that avoids row
   materialization. Refactor exec_update to use scan_table_with_callback.
5. **DB file size** at 3.5× larger — no prefix compression on B+tree keys,
   no row format optimization. SQLite uses record format with type-affinity
   byte + varint lengths; we use a simpler 1-byte-tag + fixed-length encoding.
6. **MVCC** (`Snapshot`, `VersionTracker`) is still dead code relative to
   the query path (P3.1).
7. **`Rc<RefCell<Page>>`** in the pager cache blocks `Database: Send + Sync`,
   which forces the server to use `Mutex<Database>` (no concurrent reads).

### Known correctness bugs (filed via SLT + differential suites)

1. **Predicate pushdown treats LEFT JOIN like INNER JOIN** for predicates
   on the NULL-extended right column. `SELECT ... FROM u LEFT JOIN r ON
   u.id = r.id WHERE r.amount IS NULL` returns all left rows with NULL,
   instead of only those without a match in `r`. Workaround: rephrase
   as `WHERE r.id IS NULL` (rarely works) or use a correlated NOT
   EXISTS subquery (also unimplemented — see #3). Documented in
   `tests/slt/cases/select2.test` Section 4.
2. **ORDER BY on an aggregate expression** (e.g. `SELECT region, SUM(qty)
   ... GROUP BY region ORDER BY SUM(qty) DESC`) doesn't sort. The Sort
   operator can't evaluate aggregate exprs against the Aggregate
   operator's output. Workaround: sort by the group key (e.g.
   `ORDER BY region`) or pre-compute the aggregate into a CTE/derived
   table and sort that. Documented in `tests/slt/cases/select3.test`
   Section 9.
3. **Scalar subqueries in SELECT list** (`SELECT u.name, (SELECT
   COUNT(*) FROM orders WHERE user_id = u.id) FROM users u`) — needs
   EvalContext refactor to thread `&mut Pager` + `&Catalog` into the
   per-row expression evaluator. Tracked in Phase 4.
4. **IN subqueries** (`WHERE id IN (SELECT ... )`) — same root cause as #3.

---

## Phase 0 — Harness & baseline (DONE baseline; ongoing expansion)

- [x] **P0.1** Confirm `cargo build --release` + `cargo test --release` clean (59 tests / 304 internal cases pass).
- [x] **P0.2** Confirm `cargo run --release --example bench_compare` runs end-to-end and produces the table above.
- [ ] **P0.3** Add a `benches/sqlite_compare.rs` criterion benchmark that runs the same workloads as `bench_compare` and writes baseline JSON into `benches/baseline.json` for regression tracking.
- [ ] **P0.4** Add GitHub Actions workflow `.github/workflows/ci.yml` that runs `cargo test --release` and `cargo run --release --example bench_compare` on every push; upload the benchmark output as an artifact.
- [ ] **P0.5** Add `cargo tarpaulin` coverage gate (target: 70% line coverage on `src/`).

## Phase 1 — Test-parity with SQLite

> SQLite's own TCL test suite (~45k cases) and TH3 are not public. The
> public, adaptable test corpus is **SQLlogictest (SLT)** — the same shape
> SQLite itself uses for cross-engine verification. Our parity target is
> SLT plus a differential fuzzer plus expanded differential cases.

- [x] **P1.1** Build `tests/slt_runner.rs` — a parser + runner for the SLT
      file format (`statement ok`, `statement error`, `query … values…`).
      Accepts any `.test` file under `tests/slt/cases/`. ✅
- [x] **P1.2a** Vendor `select1.test` (80 cases — basic SELECT semantics). ✅
- [x] **P1.2b** Vendor `select2.test` (32 cases — joins, multi-table aggregates,
      OUTER JOIN NULL handling, self-joins, CROSS JOIN, multi-ON-conjunct
      joins, JOIN+UPDATE/DELETE, mixed INNER/LEFT, table-qualifier
      disambiguation). ✅
- [x] **P1.2c** Vendor `select3.test` (28 cases — aggregates COUNT/SUM/AVG/
      MIN/MAX with NULLs, GROUP BY single + multi-col, HAVING, DISTINCT,
      aggregate+JOIN, empty-table aggregates). ✅
- [ ] **P1.2d** Vendor additional SLT cases:
      - [ ] `index/btree/boundary1.test` … `boundary3.test`
      - [ ] `e_createtbl.test` (CREATE TABLE semantics)
      - [ ] `e_insert.test` (INSERT semantics)
      - [ ] `e_select.test` (SELECT semantics)
- [ ] **P1.3** Implement a **differential fuzzer** `tests/fuzz_differential.rs`:
      - Random SQL generator (DDL + DML + SELECT) parameterized by row count,
        column types, predicate forms, join shapes.
      - Run each generated program against `rusqlite` (oracle) and `rustqlite`.
      - Assert value-by-value equality; on mismatch, minimize and persist the
        failing case to `tests/fuzz/corpus/`.
- [ ] **P1.4** Expand `tests/differential.rs` from 50 → 500+ (currently at 164, up from 140):
      - [ ] All SQLite affinity/coercion rules (TEXT/INTEGER/REAL/BLOB/NUMERIC)
      - [ ] All collations (BINARY, NOCASE, RTRIM)
      - [ ] NULL handling in every position (WHERE, JOIN ON, aggregates, ORDER BY)
      - [ ] Allset/IN/EXISTS/NOT EXISTS subqueries
      - [ ] Window functions: ROW_NUMBER, RANK, LAG, LEAD, FIRST_VALUE, NTILE
      - [ ] Common Table Expressions: WITH, WITH RECURSIVE
      - [ ] PRAGMA: foreign_keys, journal_mode, recursive_triggers, etc.
      - [ ] Type affinity edge cases (e.g. `'1' + 1` -> 2)
      - [ ] Conflict resolutions: OR REPLACE, OR IGNORE, OR FAIL, OR ABORT, OR ROLLBACK
      - [ ] CHECK constraints, DEFAULT expressions, generated columns
      - [ ] Foreign keys: ON DELETE CASCADE/SET NULL/RESTRICT
      - [ ] Triggers: BEFORE/AFTER INSERT/UPDATE/UPDATE OF column-list
- [ ] **P1.5** Add golden-file tests for **crash recovery**: write WAL frames,
      simulate crash mid-checkpoint, reopen, verify committed data intact.
- [ ] **P1.6** Add **migration tests**: open a v0.1 database file written by
      the current release with the next release; ensure backward compat.
- [ ] **P1.7** Property-based tests via `proptest`:
      - [ ] B+tree insert/lookup/delete invariants (sorted, balanced)
      - [ ] Row codec round-trip for every value combination
      - [ ] SQL parser round-trip: parse(pretty_print(parse(sql))) == parse(sql)
- [ ] **P1.8** TPC-C (5-table, 9-transaction) scaffold as a realistic
      concurrency + correctness test. Even a subset (e.g. New-Order + Payment)
      exercises transactional semantics hard.
- [ ] **P1.9** TPC-H subset (Q1, Q3, Q5, Q10) as analytical workload tests —
      validates joins, aggregates, subqueries, window functions.

## Phase 2 — Performance parity (beat SQLite)

- [x] **P2.1** **Fix `UPDATE by PK` 743× regression** — DONE. Now 2.6× slower.
      Used `apply_where_for_scan` to route `WHERE id = ?` to RowidLookup,
      plus in-place B+tree cell overwrite for same-size payloads.
- [x] **P2.2** **Reduce per-query setup cost** — DONE. Statement cache
      (Arc<Statement> + Plan), O(1) cache hit. Was 1.5 ms per query of
      fixed overhead, now ~50 ns.
- [x] **P2.3** **Batched INSERT fast path** — DONE. Multi-row VALUES
      13× → 3.3× slower. (Cursor that retains leaf position is still P2.5.)
- [ ] **P2.4** **Auto-transaction for consecutive single-row INSERTs** —
      when the API detects N consecutive single-row INSERTs to the same table
      outside an explicit transaction, transparently wrap them in BEGIN/COMMIT.
      Mirrors SQLite's automatic batching when `synchronous=NORMAL` + WAL.
      (Largely closed by `deferred_flush` mode + O(1) dirty-page counter.)
- [ ] **P2.5** **Cursor-based range scan** — instead of materializing all
      matched rows into a `Vec<Row>` and returning them, expose a `RowIter`
      trait so the executor can pull rows lazily. Cuts peak memory and lets
      `LIMIT 10` short-circuit a 10k-row scan.
      (Partial: streaming-aggregate fast path does this for SUM/AVG/COUNT.)
- [ ] **P2.6** **Index-only scan** — for `SELECT indexed_col FROM t WHERE
      indexed_col > ?`, return rows directly from the index B+tree without
      the rowid→table lookup. Eliminates a disk seek per row.
- [ ] **P2.7** **Skip scan** — for `SELECT … WHERE indexed_col = ? AND
      unindexed_col = ?` where the leading index column is unbounded, use
      SQLite's skip-scan strategy when distinct count of leading column is
      small.
- [ ] **P2.8** **Vectorized scan + filter** — evaluate the WHERE predicate on
      a batch of decoded rows at once, not row-by-row. Cache-friendly for large
      scans; especially helps `Aggregate` and `GROUP BY`.
      (Partial: streaming-aggregate fast path bypasses materialization,
      but still uses per-row eval_row with EvalContext setup.)
- [ ] **P2.9** **Merge join** — for equi-joins where both inputs are sorted
      on the join key (e.g. both sides are index scans), use merge join
      instead of hash join. Saves hash table build cost.
- [ ] **P2.10** **Bloom filter for IN-subquery** — `x IN (SELECT y FROM t)`
      should build a bloom filter on the subquery output, then probe it in
      the outer scan. Avoids the hash table lookup per row.
- [ ] **P2.11** **Parallel scan** — for large tables on multi-core machines,
      split the B+tree leaf range across N threads and aggregate partial
      results. Big win for `COUNT(*)`, `SUM(...)`, full scans.
- [ ] **P2.12** **SIMD CRC32** — already in `crc32fast`; verify the WAL
      checksum path is using the SIMD code path and not falling back to scalar.
- [x] **P2.13** **Query plan cache** — DONE. parse + plan once per unique SQL
      text (parameterized), reuse for subsequent calls. Mirrors SQLite's
      `sqlite3_prepare_v2` + stmt cache. Arc<Statement> makes the cache hit O(1).
- [ ] **P2.14** **Coalesce single-stmt writes** — at the API level, allow
      `Database::execute_batch` to issue a single WAL flush for a sequence
      of INSERT/UPDATE/DELETE statements (already half-implemented via
      `in_transaction`; expose as the primary write path).
- [ ] **P2.15** **Streaming UPDATE for range predicates** — `UPDATE t SET
      score = score + 1 WHERE val > 5000` currently does scan → materialize
      → filter → per-row in-place update. A streaming plan that uses
      scan_table with an inline update callback would close the 23× gap.
- [ ] **P2.16** **Pre-resolve column references in the planner** — replace
      `Expr::Column { name: String }` with `Expr::ColumnIndex(usize)` at
      plan time. Eliminates per-row column-name string comparison in
      EvalContext::lookup. Would help Aggregate (5.4× gap) and any
      expression-heavy scan.

## Phase 3 — Concurrency & MVCC

- [ ] **P3.1** **Wire `Snapshot`/`VersionTracker` into `Database::query`** —
      readers should consult the WAL up to a stable frame count, giving
      snapshot isolation. Currently `mvcc.rs` is dead code relative to the
      query path.
- [ ] **P3.2** **Refactor `Rc<RefCell<Page>>` → `Arc<Mutex<Page>>`** (or
      `Arc<RwLock<Page>>`) in `Pager::cache`. Unblocks `Database: Send + Sync`
      and removes the `unsafe impl Send/Sync for State` workaround in
      `src/bin/server.rs`.
- [ ] **P3.3** **Switch server to `Arc<RwLock<Database>>`** — concurrent
      readers, exclusive writer. Combined with P3.1, gives SQLite-WAL-equivalent
      concurrency (N readers + 1 writer).
- [ ] **P3.4** **Transaction isolation levels** — implement SQLite's
      BEGIN/DEFERRED/IMMEDIATE/EXCLUSIVE semantics. Currently BEGIN/COMMIT/
      ROLLBACK exist but isolation is not enforced.
- [ ] **P3.5** **Connection pool** with WAL-mode isolation — N read
      connections + 1 write connection, mirroring SQLite's WAL writer model.
- [ ] **P3.6** **Deadlock detection** — if we ever ship row-level locks,
      implement wait-for graph detection or a timeout-based abort.
- [ ] **P3.7** **Stress test**: 100 concurrent connections, mixed read/write,
      assert no corruption and that committed reads are consistent.

## Phase 4 — SQL surface & semantics

- [ ] **P4.1** **Foreign key enforcement** — `FOREIGN KEY … REFERENCES …`,
      `ON DELETE CASCADE/SET NULL/RESTRICT/SET DEFAULT`, `ON UPDATE …`.
      Gated by `PRAGMA foreign_keys = ON`.
- [ ] **P4.2** **Triggers** — `CREATE TRIGGER … BEFORE/AFTER INSERT/UPDATE/
      DELETE ON … FOR EACH ROW WHEN … BEGIN … END`. Includes `OLD`/`NEW`
      row references and `UPDATE OF column-list` filtering.
- [ ] **P4.3** **Views** — `CREATE VIEW … AS SELECT …`, view resolution in
      the planner (expand view body at plan time).
- [ ] **P4.4** **Common Table Expressions** — `WITH … AS (…), … SELECT …`,
      including `WITH RECURSIVE` (the latter needs a fixpoint loop in the
      planner). Currently parsed but not executed.
- [ ] **P4.5** **Window functions** — `ROW_NUMBER() OVER (PARTITION BY …
      ORDER BY …)`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`,
      `LAST_VALUE`, `NTILE`, `SUM/AVG OVER (…)`. Plan node exists; executor
      may be partial.
- [ ] **P4.6** **Prepared statements + bound parameters** — `?` and `:name`
      positional/named parameter binding, stmt cache, `EXPLAIN` output.
- [ ] **P4.7** **PRAGMA surface** — implement at minimum: `foreign_keys`,
      `journal_mode`, `synchronous`, `cache_size`, `page_size`, `auto_vacuum`,
      `encoding`, `recursive_triggers`, `defer_foreign_keys`, `table_info`,
      `index_list`, `database_list`, `wal_checkpoint`, `integrity_check`,
      `optimize`.
- [ ] **P4.8** **CHECK constraints** — `CHECK (expr)` on column + table
      level. Evaluate on INSERT and UPDATE.
- [ ] **P4.9** **DEFAULT expressions** — currently defaults are evaluated;
      extend to support `DEFAULT CURRENT_TIMESTAMP`, `DEFAULT (random())`, etc.
- [ ] **P4.10** **Generated columns** — `GENERATED ALWAYS AS (expr) STORED/
      VIRTUAL`.
- [ ] **P4.11** **ALTER TABLE** — `RENAME TABLE`, `RENAME COLUMN`, `ADD
      COLUMN`, `DROP COLUMN` (the latter requires a table rebuild).
- [ ] **P4.12** **Upsert** — `INSERT … ON CONFLICT (col) DO UPDATE SET …`
      and `DO NOTHING`. Currently `OR REPLACE`/`OR IGNORE` exist; upsert is
      the more general form.
- [ ] **P4.13** **RETURNING** — `INSERT … RETURNING *`, `UPDATE … RETURNING
      …`, `DELETE … RETURNING …`.
- [ ] **P4.14** **Common SQL functions** — `COALESCE`, `IFNULL`, `NULLIF`,
      `LENGTH`, `LOWER`, `UPPER`, `SUBSTR`, `TRIM`, `LTRIM`, `RTRIM`,
      `REPLACE`, `ROUND`, `ABS`, `RANDOM`, `DATE`, `TIME`, `DATETIME`,
      `STRFTIME`, `JULIANDAY`, `PRINTF`, `INSTR`, `HEX`, `UNHEX`, `TYPEOF`,
      `QUOTE`, `GLOB`, `LIKE` (with escape), `MIN/MAX` (scalar form).
- [ ] **P4.15** **Common SQL aggregates** — `COUNT`, `SUM`, `AVG`, `MIN`,
      `MAX`, `GROUP_CONCAT`, `TOTAL`. Plus `DISTINCT` modifier on each.
- [ ] **P4.16** **EXPLAIN** — `EXPLAIN …` and `EXPLAIN QUERY PLAN …` should
      emit the planned operator tree for debugging.
- [ ] **P4.17** **SAVEPOINTs** — `SAVEPOINT name`, `RELEASE name`, `ROLLBACK
      TO name`. Nestable.

## Phase 5 — Storage & recovery

- [ ] **P5.1** **On-disk format compatibility** — either adopt SQLite's
      `"SQLite format 3\0"` magic + page format so existing `.db` files work,
      OR (more realistic) provide `Database::import_sqlite(path)` and
      `Database::export_sqlite(path)` shims that re-stream data. The latter
      avoids committing to byte-compatibility forever.
- [ ] **P5.2** **Crash-safe WAL** — ARIES-style redo/undo. We already have
      per-frame CRC32 + salt + running checksum; verify the recovery path
      replays correctly after a simulated crash mid-commit.
- [ ] **P5.3** **Checkpoint strategies** — `PRAGMA wal_checkpoint(PASSIVE |
      FULL | RESTART | TRUNCATE)`. Currently only one mode.
- [ ] **P5.4** **Online backup** — `Database::backup(target)` API mirroring
      SQLite's `sqlite3_backup_*`. Allows hot backup without blocking writers
      for the full duration.
- [ ] **P5.5** **VACUUM** — full and incremental. Rebuilds the file to
      reclaim free pages.
- [ ] **P5.6** **Page reuse list** — free-page tracking so DELETEs don't
      bloat the file. SQLite uses a free-page list in page 1.
- [ ] **P5.7** **Partial index** — `CREATE INDEX … WHERE …`. Index only
      contains rows matching the predicate.
- [ ] **P5.8** **Covering index** — `INCLUDE` columns in the index payload
      so the executor can satisfy queries from the index alone.
- [ ] **P5.9** ** WITHOUT ROWID tables** — `CREATE TABLE … WITHOUT ROWID`
      uses the PK as the cluster key, like a Postgres clustered table.

## Phase 6 — Productionization

- [ ] **P6.1** **Error type overhaul** — replace `Box<dyn Error>` with a
      `thiserror`-derived enum; preserve error context (SQL text, line, span).
- [ ] **P6.2** **Structured logging via `tracing`** — configurable log levels
      per module (pager, planner, executor); JSON output for production.
- [ ] **P6.3** **Metrics** — query latency p50/p95/p99, cache hit rate,
      WAL size, transactions/sec, active connections. Expose via `/metrics`
      on the server.
- [ ] **P6.4** **CLI improvements** — `.tables`, `.schema`, `.explain`,
      `.timer`, `.mode csv/json/table`, `.import file table`, `.dump`,
      `.restore`, `.read script.sql`. Mirror the `sqlite3` CLI surface.
- [ ] **P6.5** **Driver ecosystem** — at minimum, an `sqlx`-compatible
      adapter crate so Rust apps can swap `rusqlite` for `rustqlite` with a
      feature flag. Stretch: SeaORM and Diesel backends.
- [ ] **P6.6** **Docs** — user guide (mdbook), embedding guide, migration
      guide from SQLite, perf tuning guide, query plan explainer.
- [ ] **P6.7** **`cargo-fuzz` targets** — parser fuzzer, storage fuzzer,
      SQL semantics fuzzer (with differential checking against `rusqlite`).
- [ ] **P6.8** **Semver policy** — public API of `crate::api` is the
      stability boundary. Add `#[doc(hidden)]` to internals, add
      `#[non_exhaustive]` to enums that may grow.
- [ ] **P6.9** **Benchmarks dashboard** — commit `benches/baseline.json` to
      the repo; CI runs benchmarks and uploads a comparison artifact; a
      script renders an HTML chart of regression history.
- [ ] **P6.10** **Release automation** — `cargo release` config, CHANGELOG.md
      generation from git log, GitHub Releases with built binaries for
      linux-x86_64, linux-aarch64, macOS-arm64, windows-x86_64.

## Phase 7 — Stretch goals (beat SQLite where we can)

- [ ] **P7.1** **Columnar storage mode** — for analytical tables, store data
      column-major. Big win for OLAP scans touching few columns of wide rows.
- [ ] **P7.2** **Vectorized execution** — column-batch-at-a-time evaluation
      of WHERE, projections, aggregates. Better cache locality than row-at-a-time.
- [ ] **P7.3** **`io_uring` async I/O** — on Linux, batch page reads via
      `io_uring` for sub-ms latency on cold-cache scans.
- [ ] **P7.4** **Multi-threaded parallel scan** — splits the B+tree leaf
      range across N threads; aggregates partial results. Big win for
      analytical workloads.
- [ ] **P7.5** **Compression** — per-page LZ4/zstd compression for hot/cold
      data separation. SQLite has a proprietary extension for this; we can
      ship it as core.
- [ ] **P7.6** **Materialized views** — auto-maintain a materialized view
      on writes to the base table. Cuts analytical query latency by 10-100×.
- [ ] **P7.7** **Query result cache** — for read-heavy workloads with
      repeated queries, cache the materialized result with a TTL invalidated
      on writes to the underlying tables.
- [ ] **P7.8** **JIT codegen** — for hot queries, codegen a specialized
      evaluator via Cranelift. Skip the interpreter dispatch overhead.

---

## Tracking conventions

- Tick a box when the work is **merged to master and CI is green**.
- For perf items, add a row to `BENCHMARKS.md` with before/after numbers.
- For test items, add a row to `TESTS.md` with case count + pass rate.
- Every commit should be small and self-contained; prefer many small
  commits over a giant "wip" push.
- After every perf-related commit, re-run `bench_compare` and update the
  table at the top of this file.

## Working order (current sprint)

1. ✅ Baseline + this TODO file.
2. ✅ Investigate and fix `exec_update` 743× regression (P2.1) — done in prior commit.
3. ✅ Add SLT runner scaffold (P1.1) + starter cases (P1.2a/b/c) — done; 140 cases.
4. ⏳ Expand differential tests to 500+ (P1.4) — at 164/500; see Phase 1 sub-items.
5. ✅ Reduce per-query overhead (P2.2/P2.13) — statement cache landed.
6. ✅ Batched-insert fast path (P2.3) — landed; multi-row VALUES 13× → 3.5×.
7. ✅ Cursor-based row iteration (P2.5) — partial; RowidRange operator lands
   range scans with seek-to-start + stop-at-end (no full materialization for
   BETWEEN). A general pull-based `RowIter` trait for all operators is still
   future work.
8. ✅ Hash join rework — smaller-side build + byte-encoded keys (Phase 2).
9. ✅ Range scan / RowidRange (Phase 2) — BETWEEN / > / < / >= / <= on rowid
   now uses `Btree::scan_table_range`. IndexRange (range scans on secondary
   indexes) is still future work.
10. ⏳ Auto-txn for consecutive single-row INSERTs (P2.4).
11. ⏳ IndexRange — range scans via secondary index (mirror RowidRange).
12. ⏳ Vectorized scan + filter (P2.8) — to close the 5.7× aggregate gap.
13. ⏳ Fix predicate pushdown for LEFT JOIN NULL-extended predicates (bug #1).
14. ⏳ Fix ORDER BY on aggregate expressions (bug #2).
15. ⏳ Refactor `Rc<RefCell<Page>>` → `Arc<Mutex<Page>>` (P3.2) — unblocks
    true concurrent reads via `Arc<RwLock<Database>>` (P3.3).
16. ⏳ Wire MVCC visibility (P3.1) — `Snapshot`/`VersionTracker` are still
    dead code relative to the query path.
17. ⏳ Scalar subqueries in SELECT list + IN/NOT IN subqueries (bug #3, #4) —
    needs `EvalContext` refactor to thread `&mut Pager` + `&Catalog`.
18. ⏳ Push to `iamleson98/rust-sql`, update `worklog.md` — ongoing after each batch.
