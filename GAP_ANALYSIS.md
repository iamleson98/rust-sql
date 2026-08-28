# Gap Analysis — Where rustqlite Still Loses to SQLite

> Baseline re-measured `2026-08-28` after commits `3124543` (date/time +
> UPSERT + RETURNING + CHECK/NOT NULL + implicit UNIQUE indexes), `3614c8a`
> (uncorrelated subqueries), `36864ab` (index B+tree sorted by (key,rowid),
> order-preserving index keys), `bcefb50` (IndexRange plan node + BTREE_APPEND
> regression fix). `cargo test --release`: 108 tests passing.
> Workload: `cargo run --release --example bench_compare`.

## 1. Current performance gap (lower ratio = closer to SQLite)

**Wins — rustqlite beats SQLite:**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | UPDATE range (`val > 5000`, indexed) | **22 µs** | 1.27 ms | **57× faster** | ✅ IndexRange plan node (was 29.6 ms / 22× slower) |
| 2 | Aggregate (SUM/AVG/MIN/MAX) | **606 µs** | 1.20 ms | **2× faster** | ✅ vectorized fast path |
| 3 | 3-table join, filter by PK (~50 rows) | **47.9 µs** | 55.9 µs | **1.2× faster** | ✅ IndexNestedLoopJoin (was 22 ms / 177× slower) |
| 4 | Single-row inserts (1k, auto-commit) | **1.43 ms** | 1.83 ms | **1.3× faster** | ✅ deferred flush (was 6.9× slower) |
| 5 | Binary size (stripped) | **1.25 MB** | 1.99 MB | **0.63×** | ✅ |

**Remaining gaps:**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 6 | Mixed 80/20 (5k ops) | 10.9 ms | 2.46 ms | 4.4× slower | 🟡 composite of gaps below |
| 7 | 2-table join + GROUP BY (full scan) | 13.3 ms | 3.01 ms | 4.4× slower | 🟡 hash agg / vectorization gap |
| 8 | Multi-row VALUES batches (10k) | 17.5 ms | 6.66 ms | 2.6× slower | 🟢 per-row insert overhead |
| 9 | Point lookup by rowid (1k ops) | 955 µs | 361 µs | 2.6× slower | 🟡 per-query overhead |
| 10 | Single-row in BEGIN/COMMIT (100k) | 205 ms | 129 ms | 1.6× slower | 🟢 was 4× (append-path fix) |
| 11 | GROUP BY (100 buckets) | 2.99 ms | 1.95 ms | 1.5× slower | 🟢 |
| 12 | Point lookup by indexed col (1k ops) | 609 µs | 543 µs | 1.1× slower | 🟢 near parity (was O(N) scan) |
| 13 | Range scan (1000 rows) | 211 µs | 119 µs | 1.8× slower | 🟢 |
| 14 | Range scan (5000 rows) | 1.18 ms | 576 µs | 2.0× slower | 🟢 |
| 15 | Full scan + COUNT with filter | 1.35 ms | 504 µs | 2.7× slower | 🟡 vectorization gap |
| 16 | DB file size (10k rows) | 917.5 KB | 262.1 KB | 3.5× larger | 🟡 free-page reuse + row codec |
| 17 | Peak RSS (100k insert) | 27.9 MB | 21.5 MB | 1.3× larger | 🟢 |
| 18 | DELETE by PK (1k ops) | 4.24 ms | 1.49 ms | 2.8× slower | 🟡 |
| 19 | UPDATE by PK (1k ops) | 5.53 ms | 1.84 ms | 3.0× slower | 🟡 |
| 20 | Range scan (10 rows) | 43 µs | 3.3 µs | 13× slower | 🔴 fixed per-query overhead dominates |
| 21 | 2-table join (filter by PK, ~10 rows) | 106 µs | 20 µs | 5.2× slower | 🟡 |

## 2. What was closed this sprint (2026-08-28)

### Storage engine
- **Index B+tree now sorted by (key, rowid)** with an order-preserving key
  encoding (NULL < int/real interleaved numerically < text < blob).
  `lookup_index` is an O(log N) seek + prefix scan (was a full O(N) scan
  of every page); interior splits are type-preserving and propagate
  separators correctly, so multi-level index trees work.
- **IndexRange plan node**: `WHERE indexed_col > ?` / `BETWEEN` now seeks
  the index and fetches only matching rows. This alone took the
  UPDATE-range workload from 29.6 ms to 22 µs (57× faster than SQLite).
- **BTREE_APPEND fast path restored** (a regression from constraint
  enforcement pre-assigning rowids silently disabled it — 100k-row
  in-txn inserts went 166 → 670 → 205 ms).

### SQL surface (from SQLite's date.c, upsert.c, returning research)
- **Full date/time engine**: date/time/datetime/julianday/unixepoch/
  strftime/timediff with all modifiers (`+N days`, `start of month`,
  `weekday N`, `unixepoch`, `localtime`/`utc` via a built-in TZif reader,
  `subsec`, `ceiling`/`floor`, `end of ...`) and every strftime specifier.
  Replaced the 1970-01-01 stubs.
- **UPSERT**: `ON CONFLICT (target) DO NOTHING / DO UPDATE SET` with
  `excluded.*` refs, WHERE guard bound to the pre-update row, targeted
  unique-index / rowid-PK matching, in-place B+tree rewrite.
- **RETURNING** for INSERT/UPDATE/DELETE (post-change rows; pre-delete
  rows for DELETE), wired through `Database::query`.
- **CHECK constraints** (column + table level) and **NOT NULL**
  enforcement on all write paths.
- **Implicit UNIQUE indexes**: column-level UNIQUE, table-level UNIQUE,
  and non-rowid-alias PRIMARY KEY now create `sqlite_autoindex_<t>_<n>`
  (previously silently unenforced). DROP TABLE cleans up their pages and
  schema rows.
- **Uncorrelated subqueries**: scalar / IN / EXISTS execute once per
  statement and substitute results (mirrors SQLite's OP_Once), including
  arbitrary nesting. Correlated subqueries fail cleanly.

## 3. Working order (next sprint)

1. ⏳ Per-query overhead (~1 µs fixed cost) — point lookups, small range
   scans, and the mixed workload are dominated by it. Ideas: cache the
   `plan_has_subqueries` flag on the statement cache; skip root_overrides
   clones for read-only plans; specialize `Plan::RowidLookup` to avoid
   generic ExecResult materialization.
2. ⏳ Vectorized scan + hash aggregation — full-scan COUNT, join+GROUP BY.
3. ⏳ In-place UPDATE for same-size payloads on the non-streaming path.
4. ⏳ Free-page reuse + row codec compaction — file size 3.5× gap.
5. ⏳ Correlated subqueries (per-row execution with outer-row binding).
6. ⏳ Views, triggers, CTE execution, window LAG/LEAD.
7. ⏳ MVCC visibility wiring + connection pool for concurrency parity.

---

## 4. Tracking conventions

- Tick a box when the work is **merged to master and CI is green**.
- For perf items, re-run `bench_compare` and update the table at the top
  of this file.
- For test items, add a row to `TESTS.md` with case count + pass rate.
- Every commit should be small and self-contained; prefer many small
  commits over a giant "wip" push.
- After every perf-related commit, re-run `bench_compare` and update
  `PRODUCTION_TODO.md` baseline.
