# Gap Analysis — Where rustqlite Still Loses to SQLite

> Re-measured `2026-08-29` after the fixed-cost elimination sprint
> (statement pipeline overhaul): zero-allocation lexer (binary-search
> keyword recognition, static operator strings), "cache on second sight"
> statement admission, mimalloc global allocator, rewritten hash join
> (pure-equi fast path, u64 numeric-key path, bucket-chain table, fused
> projection), lazy write-back for `:memory:` databases, identity
> projection fast path, and a dedicated fast path for single-row literal
> INSERTs (byte-level scanner — no tokenizer/AST/Plan). `cargo test
> --release`: 107 unit tests + 198 differential cases + 7 integration
> suites, all green. Benchmark: `cargo bench --bench sqlite_comparison`.

## 1. Current performance gap (lower ratio = closer to SQLite)

**Head-to-head (`cargo bench --bench sqlite_comparison`, criterion, 2026-08-29):**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | INSERT auto-commit (1k, unique SQL text) | **1.08 ms** | 1.55 ms | **1.44× faster** | ✅ fast INSERT scanner (was 3.6× slower at sprint start) |
| 2 | INSERT in BEGIN/COMMIT (1k, unique SQL text) | 1.14 ms | 1.07 ms | 1.07× slower | 🟡 near parity (was 2.9× slower; regressed slightly from the Arc-map durability bookkeeping — worth reclaiming) |
| 3 | Point lookup by rowid | **656 ns** | 1.65 µs | **2.51× faster** | ✅ |
| 4 | COUNT(*) on 10k rows | **1.01 µs** | 2.61 µs | **2.57× faster** | ✅ vectorized scan count |
| 5 | Concurrent read throughput (8 readers) | **4.83 ms** | 15.1 ms | **3.14× faster** | ✅ MRMW readers + Arc snapshot maps (was 2.3×) |
| 6 | Mixed read/write (4 readers + 1 writer) | **1.76 ms** | 4.91 ms | **2.79× faster** | ✅ |
| 7 | Range scan (100 rows) | **10.5 µs** | 11.2 µs | **1.07× faster** | ✅ identity projection (was 1.7× slower) |
| 8 | Inner join (1k × 1k, rowid FK) | 155 µs | 122 µs | 1.27× slower | 🟡 hash join rewrite closed 6.2× → 1.27×; remainder = row materialization |

Earlier `bench_compare` workloads (single-shot, warmed steady state),
re-measured after the index-path sprint (`COUNT` via index now covers,
index navigation is allocation-free, big index-range selections use a
merge scan, streaming UPDATE fuses fetch + index maintenance):

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 9 | COUNT(*) with indexed range filter | **32 µs** | 479 µs* | **~15× faster** | ✅ covering-index count (*SQLite runs its filtered count BEFORE creating the index — its indexed number is not measured; the comparison uses its full-scan count) |
| 10 | UPDATE range (`score = score+1 WHERE val > 5000`, indexed) | 2.3 ms | 1.3 ms | 1.8× slower | 🟡 was "59× faster" — that number timed an empty index (pre-backfill bug); real work now measured; merge scan + fused maintenance closed 5.2→2.3 ms |
| 11 | 3-table join, filter by PK (~50 rows) | 156 µs | 62 µs | 2.5× slower | 🟡 was "15× faster" — also inflated by the empty-index bug; real number now |
| 12 | UPDATE by PK (1k ops) | **1.32 ms** | 1.85 ms | **1.4× faster** | ✅ |
| 13 | Aggregate (SUM/AVG/MIN/MAX) | **567 µs** | 1.22 ms | **2.2× faster** | ✅ |
| 14 | GROUP BY (100 buckets) | **1.86 ms** | 2.04 ms | **1.1× faster** | ✅ |
| 15 | 2-table join + GROUP BY (full scan) | **2.44 ms** | 3.45 ms | **1.4× faster** | ✅ |
| 16 | Range scan (100 rows) | **17 µs** | 18.5 µs | **1.1× faster** | ✅ |
| 17 | Binary size (stripped) | **1.53 MB** | 2.00 MB | **0.77×** | ✅ |
| 18 | Point lookup by rowid (1k ops) | 776 µs | 350 µs | 2.2× slower | 🟡 per-query floor |
| 19 | Point lookup by indexed col (1k ops) | 1.31 ms | 535 µs | 2.4× slower | 🟡 index seek + row fetch |
| 20 | DELETE by PK (1k ops) | 1.92 ms | 1.34 ms | 1.4× slower | 🟡 |
| 21 | Mixed 80/20 (5k ops) | 4.21 ms | 2.51 ms | 1.7× slower | 🟡 composite |
| 22 | Range scan (5000 rows) | 927 µs | 569 µs | 1.6× slower | 🟢 |
| 23 | Multi-row VALUES batches (10k) | 12.3 ms | 6.4 ms | 1.9× slower | 🟢 |
| 24 | Full scan + COUNT with filter (no index) | 1.18 ms | 479 µs | 2.5× slower | 🟡 per-row filter eval |
| 25 | DB file size (10k rows) | 327.7 KB | 262.1 KB | 1.25× larger | 🟢 |

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

## 4. What was closed this sprint (2026-08-29, second pass): the statement pipeline

The single biggest remaining cost for OLTP-shaped workloads was the fixed
per-statement pipeline: tokenize → parse → plan → (maybe) cache → execute.
Profiling a 1k-row in-transaction INSERT batch showed ~3.1 µs/insert split
roughly: parse 1.2 µs, plan 0.16 µs, cache-populate 1.0 µs, execute 1.3 µs.
Every phase was attacked:

### Lexer: zero-allocation tokens
- `Token::Keyword(&'static str)` and `Token::Op(&'static str)` — keywords
  and operators are static strings now; previously every keyword token
  heap-allocated its uppercase spelling, and `try_multi_char_op` called
  `format!()` twice for EVERY operator token (dead 3-char probe included).
- Keyword recognition is a binary search over the sorted static table with
  case-folding comparison (~8 short memcmps). Previously: a
  `to_ascii_uppercase()` String allocation + linear scan over 144 keywords
  per identifier (~900 string compares per INSERT statement).
- **UTF-8 correctness fix**: string and quoted-identifier literals were
  built with `byte as char`, silently mangling multi-byte text
  (`'héllo'` stored as `"hÃ©llo"`). Now byte-collected and decoded once.

### Statement cache: cache on second sight
- Populating the cache costs ~1 µs (two key Strings, write lock, Arc
  clones, FIFO bookkeeping). For one-off statement text — literal-inlined
  `INSERT ... VALUES ('name42', 42)` — that is pure waste. A 5 ns FxHash
  "seen" filter admits a statement to the cache only on its second
  sighting.

### Allocator: mimalloc
- The engine allocates on every hot path (row Vecs, ASTs, join keys).
  mimalloc's thread-local heaps cut small-allocation cost 20–40%
  (feature `mimalloc`, on by default; opt out with
  `default-features = false`). Full-join benchmark: 337 → 229 µs from
  this change alone.

### Hash join: rebuilt for the common case
- **Pure-equi fast path**: when the join condition is exactly the
  equi-key predicates, a hash-key match PROVES the condition — the
  per-candidate `eval_row` (AST walk + string column resolution,
  ~300–500 ns/row) is skipped entirely.
- **u64 numeric-key path**: single-column joins where every build key is
  a small INTEGER or REAL hash by the 8-byte order-key double — no
  per-row `Vec<u8>` key allocation, and Integer(5) / Real(5.0) match
  exactly per SQL numeric equality (the old tagged encoding silently
  dropped cross-type matches — a correctness fix).
- **Bucket-chain hash table** (`HashMap<K, u32>` + flat chain Vec): one
  allocation per build row instead of two.
- **Fused projection**: `Project(HashJoin)` emits only the projected
  columns directly — no full-width combined row, no second cloning pass.
- NULL join keys never match (SQL semantics), with correct unmatched-row
  emission for LEFT/RIGHT/FULL.
- Identity projection: `SELECT *` (or any in-order full projection) moves
  rows instead of cloning them (1000 allocations per 1000-row scan gone).

### Storage: lazy write-back for `:memory:`
- In-memory databases no longer write pages to the backing temp file on
  every auto-commit statement: `flush()` is a no-op and dirty pages spill
  only on cache eviction. BEGIN forces one real write-back so ROLLBACK
  (which restores by clearing the cache and re-reading the file) stays
  correct. Auto-commit INSERT: 4.8 ms → 2.0 ms per 1k batch.
- `insert_table_append`: one page-lock acquisition per level instead of
  five; cell bytes built in a 256-byte stack buffer (heap only for big
  rows).
- Root/max-rowid bookkeeping no longer allocates key Strings when values
  are unchanged; `sync_schema_roots` runs only when a root actually moved
  (was: two locks + two Vec collects per statement).

### Fast INSERT path (the endgame for item 1)
- A byte-level scanner recognizes `INSERT INTO t (cols) VALUES (literals)`
  — single row, literals only — and executes it without building tokens,
  an AST, or a Plan, funneling into the very same
  `exec_insert_one_row` (so affinity, NOT NULL, UNIQUE-index maintenance,
  conflict handling, and rowid semantics are identical to the general
  path). Result: **0.92 µs/insert**, beating SQLite's 1.05 µs on the
  unique-SQL-text benchmark where statement caching is useless.

## 5. What was closed this sprint (2026-08-29, third pass): the index paths

Benchmarks after the statement-pipeline sprint exposed that several
previously "winning" index workloads had been timing a NO-OP: `CREATE
INDEX` never backfilled existing rows (fixed in 7e56b7f), so the index was
empty and `UPDATE ... WHERE val > 5000` matched zero rows. With the index
actually populated, the real index-path costs surfaced — and were attacked:

### Allocation-free index B+tree navigation
- Interior index pages were navigated by decoding EVERY cell into an
  allocating `Cell` (each index cell owns a `Vec<u8>` key — ~150-200 heap
  allocations per interior page, per level, per descent). All navigation
  (insert descent, delete descent, range scan, full scan) now uses
  `IndexCellView` — a borrow-only view of the cell's key/rowid/child — and
  BINARY SEARCH instead of linear scans. Table interior pages get the same
  treatment (`decode_table_interior_key/child`).
- Page cache and dirty-page set keyed by u32 with a splitmix64 hasher
  (~2 ns vs ~20-25 ns for std's SipHash per lookup).

### Merge-scan rowid fetching
- `exec_index_range` and the streaming UPDATE fetched each matching row
  with an independent B+tree descent (~300 ns each, random order). When
  the selection exceeds ~25% of the table (via the cached max-rowid
  estimate), the rowids are now sorted and the table scanned ONCE with a
  merge cursor (~60-80 ns per visited row), preserving the original
  emission order via a position map. Selecting 5000 of 10000 rows:
  ~1.5 ms of descents → ~0.7 ms of scan.

### Covering-index COUNT
- `SELECT COUNT(*) FROM t WHERE indexed_col > ?` now counts index ENTRIES
  directly — no row fetching, no decoding, no materialization. 835 µs →
  32 µs on a 10k-row table (26×).

### Streaming UPDATE improvements
- Old payload stashed during the scan phase (lookup-based sources already
  hold it as an owned Vec — free) instead of a per-row re-fetch descent.
- Index maintenance uses the already-encoded old/new keys directly against
  persistent B+tree handles (one per touched index), instead of
  re-encoding and re-opening the tree per row.
- Root lookups (`table_root`/`index_root`) no longer allocate a lowercase
  String per call; INLJ hoists the index root out of the per-row loop.

## 6. What was closed this sprint (2026-08-29, fourth pass): shared-map snapshots

The three per-Database bookkeeping maps (table root overrides, index
roots, max-rowid cache) were deep-cloned into every statement's
ExecContext — ~250 ns of allocations+copies per query on a post-split
database, and the dominant serializer for concurrent readers (each reader
cloned every map under a read lock).

- **Readers (`query`/`query_with_columns`)**: the maps are now
  `RwLock<Arc<HashMap>>`; a query clones three Arc snapshots (one atomic
  refcount bump each) and reads through them. Concurrent-read throughput
  improved 2.3× → 3.14× vs SQLite.
- **Writers (`execute`)**: the maps are DETACHED (zero-copy move) into
  the statement — `execute` holds `&mut self`, so no reader can race —
  mutated in place, and attached back. `Arc::make_mut` never deep-copies
  because the statement is the sole owner.
- **Statement-local overlay**: ExecContext keeps local overlay maps +
  changed flags; the write-back merges only what changed. Pure SELECTs
  can still populate the max-rowid scan cache (used by the merge-scan
  heuristic) — that merges back without touching root bookkeeping.
- **Statement cache + seen-set**: FxHash string keys (~10-15 ns vs
  SipHash's 40-80 ns per SQL text hash).
- ROLLBACK clears the shared snapshots (entries cached during the
  transaction may reference rolled-back pages).

## 7. Working order (next sprint)

1. ⏳ Inner join (1.28×) — the last criterion head-to-head gap. Scan-side
   row materialization (one Vec per row) is the remainder; a column-block
   scan or join-side row reuse would close it.
2. ⏳ DELETE by PK (3.4×) — per-statement flush dominates; batch dirty
   pages like SQLite's WAL auto-checkpoint, or make per-statement flush
   incremental (write-behind). A fast DELETE scanner (mirror of the fast
   INSERT path) is another option for `DELETE FROM t WHERE rowid = k`.
3. ⏳ Full-scan COUNT with filter (2.4×) + join+GROUP BY (2.5×) —
   predicate evaluation per row is still `eval_row` (name lookup);
   resolve predicate columns to indices like GROUP BY now does.
4. ⏳ Multi-row VALUES batches (2.5×) — per-row insert overhead
   (constraint checks, index probes).
5. ⏳ Correlated subqueries (per-row execution with outer-row binding).
6. ⏳ Views, triggers, CTE execution, window LAG/LEAD.
7. ⏳ MVCC visibility wiring + connection pool for concurrency parity.
8. ⏳ Fast-path scanners for the other hot shapes: single-row UPDATE by
   rowid / point DELETE (mirroring the fast INSERT scanner).

---

## 8. Tracking conventions

- Tick a box when the work is **merged to master and CI is green**.
- For perf items, re-run `bench_compare` and update the table at the top
  of this file.
- For test items, add a row to `TESTS.md` with case count + pass rate.
- Every commit should be small and self-contained; prefer many small
  commits over a giant "wip" push.
- After every perf-related commit, re-run `bench_compare` and update
  `PRODUCTION_TODO.md` baseline.
