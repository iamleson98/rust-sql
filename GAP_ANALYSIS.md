# Gap Analysis — Where rustqlite Still Loses to SQLite

> Re-measured `2026-08-29` after the execution-overhead + storage sprint:
> `02d5e5c` (per-query fixed overhead: cached column names, Arc<[String]>
> ExecResult, precomputed projection indices, cached has_subqueries flag),
> `bc7bc99` (vectorized GROUP BY: zero-alloc hash grouping, selective
> decode, AggFunc enum dispatch), `b3e241e` (row codec v2 + rowid-alias
> elision, append-mode B+tree splits, empty-leaf freelist recycling),
> `5f0f47d` (binary-search update_table), `6aa8e10` (steady-state bench
> warmups). `cargo test --release`: 104 tests, 191 differential cases.
> Workload: `cargo run --release --example bench_compare` (single-shot
> workloads warmed to steady state, matching SQLite's prepare-outside-
> timer methodology).

## 1. Current performance gap (lower ratio = closer to SQLite)

**Wins — rustqlite beats SQLite:**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | UPDATE range (`val > 5000`, indexed) | **21 µs** | 1.24 ms | **59× faster** | ✅ IndexRange plan node (was 29.6 ms / 22× slower) |
| 2 | 3-table join, filter by PK (~50 rows) | **3.6 µs** | 56–73 µs | **15–20× faster** | ✅ INLJ + warm statement cache (was 22 ms / 177× slower) |
| 3 | 2-table join, filter by PK (~10 rows) | **4.2 µs** | 24 µs | **5.8× faster** | ✅ INLJ (was 5.7× slower under cold-cache timing) |
| 4 | UPDATE by PK (1k ops) | **1.25 ms** | 1.82 ms | **1.5× faster** | ✅ binary-search update_table + codec v2 (was 3.0× slower) |
| 5 | Aggregate (SUM/AVG/MIN/MAX) | **555 µs** | 1.18 ms | **2.1× faster** | ✅ vectorized fast path |
| 6 | Single-row inserts (1k, auto-commit) | **981 µs** | 1.80 ms | **1.8× faster** | ✅ deferred flush (was 6.9× slower) |
| 7 | Single-row in BEGIN/COMMIT (100k) | **116 ms** | 128 ms | **1.1× faster** | ✅ was 1.6× slower |
| 8 | Single-row in BEGIN/COMMIT (10k) | **11.1 ms** | 12.5 ms | **1.1× faster** | ✅ |
| 9 | Binary size (stripped) | **1.28 MB** | 1.99 MB | **0.64×** | ✅ |
| 10 | Point lookup by indexed col (1k ops) | **539 µs** | 526 µs | **parity** (1.02×) | ✅ was 1.1× slower |
| 11 | GROUP BY (100 buckets) | **1.92 ms** | 1.88 ms | **parity** (1.02×) | ✅ vectorized hash grouping (was 1.5× slower) |
| 12 | Peak RSS (100k insert) | **47.4 MB** | 46.0 MB | **parity** (1.03×) | ✅ was 1.3× larger |

**Remaining gaps:**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 13 | DELETE by PK (1k ops) | 4.6 ms | 1.35 ms | 3.4× slower | 🟡 per-statement flush (durability default) + row materialization |
| 14 | Mixed 80/20 (5k ops) | 7.5 ms | 2.5 ms | 3.0× slower | 🟡 composite of gaps below (was 4.4×) |
| 15 | 2-table join + GROUP BY (full scan) | 7.4 ms | 3.0 ms | 2.5× slower | 🟡 join+agg materialization (was 4.4×) |
| 16 | Multi-row VALUES batches (10k) | 16.5 ms | 6.7 ms | 2.5× slower | 🟢 per-row insert overhead |
| 17 | Full scan + COUNT with filter | 1.15 ms | 472 µs | 2.4× slower | 🟡 per-row decode + eval (was 2.7×) |
| 18 | Point lookup by rowid (1k ops) | 758 ns/op | 362 ns/op | 2.1× slower | 🟡 Vec<Row> materialization + stmt-cache hash (was 2.6×) |
| 19 | Range scan (5000 rows) | 1.01 ms | 564 µs | 1.8× slower | 🟢 |
| 20 | Range scan (1000 rows) | 167 µs | 108 µs | 1.5× slower | 🟢 (was 1.8×) |
| 21 | Range scan (100 rows) | 25 µs | 12 µs | 2.0× slower | 🟢 |
| 22 | Range scan (10 rows) | 6.6 µs | 3.5 µs | 1.9× slower | 🟢 was 13× (cold-cache timing + 6.5× real overhead) |
| 23 | DB file size (10k rows) | 327.7 KB | 262.1 KB | 1.25× larger | 🟢 codec v2 + append splits (was 3.5×) |

## 2. What was closed in the previous sprint (2026-08-28)

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

## 3. What was closed this sprint (2026-08-29): execution overhead + storage

### Per-query fixed overhead (item 1 — closed)
- `ExecResult.columns` is `Arc<[String]>`; base-table operators return the
  column names cached on `Arc<Table>` with one refcount bump (was N String
  clones / `format!()` per query).
- Statement cache stores a precomputed `has_subqueries` flag — no
  per-execution plan-tree walk (which allocated a `Vec<&Expr>`).
- `exec_project` pre-resolves bare column refs to row indices once per
  query; per-row work is a Vec index + Value clone (was a linear
  case-insensitive name scan per column per row).
- `query()` skips root_overrides/max_rowids HashMap clones when empty;
  hot paths borrow `ctx.params` directly.
- Effects: point lookup 955 → 758 ns/op; range scan (10 rows)
  43 → 6.6 µs; 2-table join 106 → 4.2 µs.

### Vectorized GROUP BY (item 2 — closed)
- `HashGrouper`: SQL-semantic hash buckets (Integer(5) groups with
  Real(5.0), -0.0 with 0, NULL is its own group) + linear probe per
  bucket. Replaced per-row `format!("{:?}")`+`join("|")` String keys —
  2-4 heap allocations per row down to zero.
- Group keys / aggregate args resolved to column indices once per query;
  `decode_row_selective` decodes ONLY referenced columns when there's no
  WHERE clause. `AggFunc` enum dispatch (no per-row &str match).
- Fix: ORDER BY now resolves projection aliases + rewrites aggregate /
  group expressions (ORDER BY <alias of expression key> used to sort on
  all-NULL keys).
- Effects: GROUP BY 2.99 → 1.92 ms (parity with SQLite); join+GROUP BY
  13.3 → 7.4 ms; 8 new differential cases vs SQLite.

### Row codec v2 + page recycling (item 4 — closed)
- Size-classed integers (0 → 1 byte, i8/i16/i32/i64 → 2/3/5/9), LEB128
  text/blob lengths, rowid-alias elision (`id INTEGER PRIMARY KEY` is a
  1-byte marker materialized from the B+tree cell key). DB magic bumped
  to `RSQLDB02` (old files get a clear "unsupported format version"
  error).
- Append-mode leaf splits: a right-edge insert keeps the old page 100%
  full and gives the new page only the new cell (SQLite's balance_quick)
  — sequential inserts fill pages ~100% instead of freezing every left
  sibling at 50%.
- Empty-leaf recycling: DELETE unlinks empty leaves from the parent and
  pushes them onto the pager freelist; `allocate_page` reuses freelist
  pages before growing the file. DELETE-all + reinsert now reuses pages.
- `update_table` leaf pass is a binary search reading only the rowid
  varint (was a linear scan with a Vec allocation per cell — codec v2's
  ~1000-cell leaves had regressed UPDATE-by-PK 2×, now 1.5× FASTER than
  SQLite).
- Fix: exec_delete invalidates the cached max-rowid when the max rowid is
  deleted (rowid allocation now matches SQLite's max(existing)+1,
  restarting at 1 after DELETE-all).
- Effects: file size 917.5 → 327.7 KB (3.5× → 1.25× of SQLite);
  UPDATE by PK 5.53 → 1.25 ms; stable file size under churn.

### Benchmark methodology (item 0 — fairness fix)
- Single-shot workloads now warm the statement cache before timing;
  SQLite's harness prepares outside its timer. Previously we were charged
  a cold parse+plan per single-shot workload (the "13× range scan" and
  "5.7× join" gaps were largely this artifact).

## 4. Working order (next sprint)

1. ⏳ DELETE by PK (3.4×) — per-statement flush dominates; batch dirty
   pages like SQLite's WAL auto-checkpoint, or make per-statement flush
   incremental (write-behind).
2. ⏳ Point lookup by rowid (2.1×) — `Vec<Row>` materialization in the
   public API; a row-cursor / callback query API would remove 2-3
   allocations per query.
3. ⏳ Full-scan COUNT with filter (2.4×) + join+GROUP BY (2.5×) —
   predicate evaluation per row is still `eval_row` (name lookup);
   resolve predicate columns to indices like GROUP BY now does.
4. ⏳ Multi-row VALUES batches (2.5×) — per-row insert overhead
   (constraint checks, index probes).
5. ⏳ Correlated subqueries (per-row execution with outer-row binding).
6. ⏳ Views, triggers, CTE execution, window LAG/LEAD.
7. ⏳ MVCC visibility wiring + connection pool for concurrency parity.

---

## 5. Tracking conventions

- Tick a box when the work is **merged to master and CI is green**.
- For perf items, re-run `bench_compare` and update the table at the top
  of this file.
- For test items, add a row to `TESTS.md` with case count + pass rate.
- Every commit should be small and self-contained; prefer many small
  commits over a giant "wip" push.
- After every perf-related commit, re-run `bench_compare` and update
  `PRODUCTION_TODO.md` baseline.
