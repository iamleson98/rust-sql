# Gap Analysis — rustqlite vs SQLite

> Re-measured `2026-09-02` after the gap-close sprint (see §1b — GROUP BY
> compiled expression keys, bucket-keyed 2-way leaf cache, cursor-ix cell
> bias, the mimalloc post-storm wake drain, and the UPDATE payload-patch
> fast path). Every head-to-head workload row is now at parity or FASTER;
> the last three losing/tied rows (point lookup by rowid, GROUP BY with
> expression keys, UPDATE range) all closed. `cargo test --release`:
> 134 unit + 214 integration/differential cases (206/206 differential vs
> SQLite, all green). Benchmark: `cargo run --release --example
> bench_compare`.

## 1. Current performance (lower ratio = closer to SQLite)

**Head-to-head (`cargo run --release --example bench_compare`, 2026-09-02
gap-close close-out):**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | Single-row inserts (1k, auto-commit) | **839 µs** | 1.74 ms | **2.1× faster** | ✅ |
| 2 | INSERT in BEGIN/COMMIT (100k) | **78 ms** | 129 ms | **1.7× faster** | ✅ |
| 3 | Multi-row VALUES batches (10k) | **4.2 ms** | 6.3 ms | **1.5× faster** | ✅ |
| 4 | Point lookup by rowid (1k) | **246–285 µs** | 347–351 µs | **1.2–1.4× faster** | ✅ bucket-keyed 2-way leaf cache + cell bias + maps flag (was 1.08× slower) |
| 5 | Range scan (10 rows) | **1.0 µs** | 1.5 µs | **1.5× faster** | ✅ |
| 6 | Range scan (100 rows) | **6.9 µs** | 11.0 µs | **1.6× faster** | ✅ |
| 7 | Range scan (1000 rows) | **65 µs** | 107 µs | **1.6× faster** | ✅ |
| 8 | Range scan (5000 rows) | **330–335 µs** | 532 µs | **1.6× faster** | ✅ |
| 9 | Full scan + COUNT with filter | **374–390 µs** | 461 µs | **1.2× faster** | ✅ |
| 10 | Aggregate (SUM/AVG/MIN/MAX) | **696–758 µs** | 1.18 ms | **1.6× faster** | ✅ |
| 11 | GROUP BY (100 buckets) | **703–721 µs** | 1.84 ms | **2.6× faster** | ✅ compiled expression keys (was 1.04× slower — the bench keys on `val/100`, an EXPRESSION) |
| 12 | Point lookup by indexed col (1k) | **304–310 µs** | 522–532 µs | **1.7× faster** | ✅ (was 1.26× faster) |
| 13 | 2-table join (filter by PK) | **2.0–2.6 µs** | 2.7–3.8 µs | **1.1–1.5× faster** | ✅ |
| 14 | 3-table join (filter by PK) | **13.7–17.3 µs** | 21.5–21.8 µs | **1.3–1.6× faster** | ✅ (was 1.04× slower) |
| 15 | 2-table join + GROUP BY (full scan) | **1.75–1.80 ms** | 2.89–2.93 ms | **1.6× faster** | ✅ |
| 16 | UPDATE by PK (1k ops) | **1.50–1.69 ms** | 1.84–1.96 ms | **1.2× faster** | ✅ |
| 17 | UPDATE range (val > 5000, 5k rows) | **1.11 ms** | 1.14 ms | **1.03× faster** | ✅ payload-patch fast path (was a tie) |
| 18 | DELETE by PK (1k ops) | **571–599 µs** | 1.34–1.37 ms | **2.3× faster** | ✅ |
| 19 | Mixed 80/20 (5k ops) | **1.99–2.00 ms** | 2.43–2.45 ms | **1.2× faster** | ✅ |
| 20 | DB file size (10k rows) | 270.3 KB | 262.1 KB | 1.03× larger | 🟢 `PRAGMA page_size=4096` matches SQLite EXACTLY (262144 B); the 8 KiB default buys the range-scan/join wins |
| 21 | Stripped binary size | 2.36 MB | 2.04 MB (est.) | 1.16× larger | 🟢 mimalloc (~140 KiB) buys the 1.5–2× write wins; no-default-features build is 2.17 MB |
| 22 | Peak RSS (100k insert+count) | **32.9 MB** | 35.6 MB | **0.92×** | ✅ |
| 23 | File-backed commit throughput (WAL/NORMAL) | **25.3 µs/txn** | 28.5 µs/txn | **1.13× faster** | ✅ `examples/bench_wal`; delete mode is 6.2× faster |
| 24 | Concurrent reads (8 threads, criterion) | **1.94 ms** | 16.1 ms | **8.3× faster** | ✅ per-page locks vs a serialized connection mutex |

**Every row is now at parity or faster.** The two 🟢 resource rows are
deliberate tradeoffs, not gaps: DB file size matches SQLite exactly with
`PRAGMA page_size=4096` (the 8 KiB default is what buys the 1.2–1.6×
range-scan/join wins), and the binary's ~140 KiB of mimalloc is what
buys the 1.5–2.1× write-throughput wins (a no-default-features build is
2.17 MB).

## 1b. What was closed in the 2026-09-02 sprint

The 2026-09-01 baseline re-measurement on this machine exposed three
losing rows and one tie: point lookup by rowid (375-402 µs vs 350-369 —
1.08× slower), GROUP BY with an EXPRESSION key (`GROUP BY val/100`:
1.92-2.02 ms vs 1.84 — the bare-column vectorized path never fired),
3-table join (22.5 vs 21.6 µs — 1.04× slower), and UPDATE range (an
exact tie). All four closed:

### GROUP BY: compiled expression keys
- The bench's `GROUP BY val / 100` is an arithmetic EXPRESSION, not a
  bare column — it fell off the vectorized selective-decode path onto
  the per-row `eval_row` AST walk (~60-120 ns/row) + full-row decode.
  `compile_expr_scoped` (a scoped variant of the UPDATE-SET compiler:
  resolves `t.col`-qualified refs against the table/alias) now compiles
  GROUP-BY keys and aggregate args once per statement into positional
  trees. When every key/arg compiles: selective decode through
  `decode_row_selective_wide` (full-width buffer for identity indexing,
  only referenced columns decoded), `intern_one` for single-key queries
  (no key-slice Vec), FxHash group hashing (~5-10 ns vs SipHash
  ~25-40 ns), and a shared static for COUNT(*)'s placeholder argument.
  2021.8 → 703-721 µs (2.6× faster than SQLite's 1.84 ms).
- 8 new differential cases (expression keys, multi-key, computed args).

### Point lookup by rowid: bucket-keyed leaf cache + cell bias
- The old 1024-slot direct-mapped cache keyed slots by `rowid & 1023` —
  a first-visit sweep over 1000 sequential rowids descends the
  root→interior→leaf path ~1000 times (one prime per rowid, and each
  prime can be evicted before reuse). Slots are now keyed by
  `rowid >> 8` (256-rowid buckets, 2-way set-associative pairs): one
  descent primes a whole bucket, so the sweep descends ~once per leaf
  pair. Fill policy: refresh the same-page entry in place, prefer
  empty/stale slots, else evict the FARTHER leaf (a bucket straddling
  a leaf boundary keeps both leaves — no thrash).
- **Cursor-ix bias** (SQLite's technique): each hint slot remembers the
  cell index of the last successful lookup; `biased_rowid_search`
  probes it first, then a bracketed binary/gallop search. Sequential
  rowids resolve in 1-2 probes vs log2(cells) ≈ 8-10. Same treatment
  for index-leaf lookups (`lookup_index_leaf` biased lower-bound).
- `maps_populated` atomic flag: the fast path skips the bookkeeping-maps
  read-lock until a split actually moves a root.
- 375-402 → 246-285 µs (SQLite: 347-351). The 3-table join improved to
  13.7-17.3 vs 21.5-21.8 µs as a side effect (INLJ inner probes hit the
  bucket cache + bias).

### The mimalloc post-storm wake (first-query latency)
- The bench's point-lookup section timed the FIRST query after a 10k-row
  insert transaction: 269-287 µs of pure allocator wake (mimalloc's
  deferred-free page-acquisition sweep), absent without mimalloc
  (examples/probe_first_bisect). The wake is once-per-process and
  RE-ARMS if drained mid-transaction (probe_mid_drain: mid-storm drain
  leaves 212 µs on the next read; post-storm leaves 19.5 µs). The
  engine now estimates freed blocks per write statement and drains at
  write-burst completion (COMMIT / auto-commit / DDL): first-query
  287 → 41 µs; the ~170 µs drain amortizes inside the multi-ms write
  transaction.

### UPDATE range: payload-patch fast path
- `UPDATE t SET score = score + 1.0 WHERE val > 5000` re-encoded every
  matched row through decode-all → copy → coerce → encode-all. When the
  table has no NOT NULL/CHECK/enforced-FK constraints, no RETURNING, and
  every SET compiles: the new payload = a byte copy of the old payload
  with the assigned columns' regions patched in place
  (`row_column_regions_into` walks the encoded layout; valid whenever
  each encoded new value keeps its size — e.g. REAL+REAL→REAL, or int
  size-class-stable increments). Size changes fall back to the generic
  path (identical semantics). 8 new differential cases cover same-size
  REALs, size-changing TEXT, int size-class boundaries (127→128,
  32767→32768), multi-assign, and NULL transitions. 1.14 ms (tie) →
  1.11 vs 1.14 ms.

## 1c. What was closed in the 2026-08-30 sprint

### Second close-out: UPDATE/aggregate fixed costs
- **Compiled residual predicates on the streaming UPDATE path**: the
  WHERE clause of `UPDATE t SET ... WHERE <pred>` was evaluated with the
  general `eval_row` AST walk (~60–120 ns/row: name resolution +
  type coercion per comparison) while the SELECT Filter path had enjoyed
  a compiled positional evaluator for a while. The residual is now
  compiled ONCE per statement (`compile_predicate`) and evaluated
  positionally against the full table-order row buffer through a
  compile-time `IDENTITY_POSITIONS` table (no per-statement `(0..n)
  .collect()` Vec). Scan-only UPDATE (0 matching rows) over 10k rows:
  1.30 → 0.87 ms; full 7.5k-row UPDATE 1.70 → 1.28 ms.
- **`Btree::count_rows_range`**: `SELECT COUNT(*) FROM t WHERE id
  BETWEEN ? AND ?` fell to the general aggregate path, which materialized
  every row in the range (full payload decode + a Vec of Values per row)
  just to count them. The new B+tree primitive binary-searches each leaf
  for the first rowid >= start and counts cells until rowid > end — zero
  payload decodes, zero allocations. 10.2 → 5.1 µs for a 100-row range
  (SQLite: 11.4 µs).
- **Prefetch in `scan_subtree_borrowed`**: the borrowed full-scan leaf
  loop (the UPDATE streaming source, the aggregate streaming scan, and
  DELETE) now prefetches the leaf's first 1 KiB after taking the page
  lock, matching what `scan_range_subtree_borrowed` and the lookup
  descent already did.
- **probe_idx_b example fixed**: a pre-existing borrow-checker error in
  the example (guard moved inside a loop) prevented `cargo test` from
  building all targets; the walker now collects child ids under the
  guard and recurses after dropping it.

### Latency (the "first query" spike)
- **mimalloc purge disabled** (`mi_option_set(purge_delay, -1)`): the
  default 10 ms delayed purge madvises freed pages, and the first
  allocation after an idle window re-faults them — 10–570 µs on the first
  query after any alloc/free storm. glibc (SQLite's allocator) never
  returns small-object pages; we match that. Verified by probe_spike2:
  storm+idle spike 570 → 21 µs (the residual is cache-cold wake, which
  the system-allocator control build shows too).

### Point-lookup fixed cost (the 1.4× indexed-lookup gap)
- **Leaf hints** (SQLite-style cursor hints): a per-thread advisory cache
  of the last-visited leaf per tree root, holding the `PageRef` itself. A
  point lookup whose key falls in the remembered bounds touches ONE page
  instead of one per level. Invalidated by a write EPOCH — `Pager::note_write`
  bumps a version on every mutation, `rollback_to` bumps it on restore,
  and a per-pager instance id prevents cross-database aliasing.
- **`lookup_table_with`**: fast paths decode the projected row under the
  page lock — no intermediate payload Vec copy.
- **Statement cache**: entries are `Arc<CachedStmt>` (1 refcount bump per
  hit) + a last-statement memo (read-lock + memcmp) for the dominant
  same-SQL loop pattern.
- **`Params::as_slice`**: array/Vec parameters bind directly — no
  per-query Vec collect.
- **Index interior fix**: the descent re-locked the parent per child; now
  the first child pointer is read under the same guard. (Also fixed a
  latent bug: interior index pages entered mid-scan visited only the
  right-most child, dropping left children — reachable in 3+ level index
  trees.)

### UPDATE paths
- **Single-row UPDATE fast path** (`UPDATE ... WHERE id = ?`): fetch
  (leaf-hinted) → decode → SET → constraints/FK → encode → in-place patch
  in one pass, skipping six per-statement Vecs. 2.34 → 1.53 ms per 1k ops.
- **In-place writes don't invalidate hints**: `note_write_in_place` keeps
  the write epoch stable for same-size payload patches (leaf bounds can't
  move), so consecutive UPDATE-by-PK statements keep hitting leaf hints.
- **Compiled SET expressions**: `score = score + 1.0` compiles once per
  statement to a positional tree (~5–15 ns/row vs the ~80–120 ns AST walk
  + name resolution).
- **Payload arena**: phase-2 update payloads share one arena buffer
  instead of one `Vec<u8>` per row.

### JOINs
- **INLJ fused projection** (mirrors the hash-join fusion): Project over
  an IndexNestedLoopJoin emits only the projected columns; the inner row
  decodes under the page lock; the defensive per-outer-row HashSet is
  allocated only when a probe returns multiple rowids.

### Features
- **ALTER TABLE RENAME COLUMN / DROP COLUMN** — full schema-object
  rewrites (see commit 0eec01b). Includes SQLite-default non-recursive
  trigger semantics (`PRAGMA recursive_triggers`, default OFF).
- **WAL mode** (see commit 7f403c1): commit frames, WAL-served reads,
  crash recovery with commit-boundary validation, auto/manual/close
  checkpointing, `PRAGMA synchronous` honored.

### Data-loss bugs found and fixed
1. `note_dirty`'s last-noted fast path was never reset by flush — a page
   re-dirtied across flushes never re-entered the dirty set and its newer
   content never reached disk (`UPDATE t SET v=2; UPDATE t SET v=3;`
   reopen read `v=1`).
2. The single-row UPDATE fast path skipped the autocommit flush.

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

## 6b. What was closed this sprint (2026-08-29, fifth pass): rowid ranges

### `WHERE id BETWEEN ? AND ?` fast path + binary-search range walks

- **`FastPath::RowidRange`** (api.rs): the plan shape
  `RowidRange { start: Some, end: Some, residual: None }` — bare or under
  a simple column projection — skips ExecContext setup, plan dispatch and
  result plumbing and drives the B-tree directly with bound parameters.
  This is the OLTP shape `SELECT cols FROM t WHERE id BETWEEN ? AND ?`;
  the general path's ~1 µs fixed cost dominated 1-100-row scans.
- **Binary-searched leaf descent**: the range walk previously linearly
  skipped every cell below `start` in each leaf (hundreds of cell decodes
  for `BETWEEN 1000 AND 1009` on a 10k-row table); it now binary-searches
  the cell array by rowid. Interior nodes binary-search the separator
  keys too and skip all children left of `start`.
- **Early-stop propagation**: the walk previously returned `Ok(())` at
  the first cell past `end` but the subtree driver still visited every
  leaf to the right, paying a page fetch + lock per leaf for a first-cell
  check. The walk now returns `Ok(false)` (stop) vs `Ok(true)`
  (continue right), so a 10-row range on a 10k-row table touches ~3
  pages instead of ~40.
- **Projection-permutation codec fix** (row_codec.rs): a correctness bug
  found by the new fast-path tests — `decode_row_selective` required
  ascending column indices, so **any reordered projection on any fast
  path silently returned NULL for the out-of-order column**
  (`SELECT val, name FROM t WHERE id = 5` → `[35, NULL]`), and duplicate
  projections (`SELECT val, val`) dropped the duplicate. Projections are
  now decoded through a sorted-index permutation with slot mapping;
  ascending projections (the common case) keep the allocation-free path,
  single-slot decodes move the value with no clone.
- Effects (bench_compare, 10k-row table): range scan 100 rows
  36 → 11.2 µs (3.0× slower → parity); 1000 rows 225 → 97 µs (2.1×
  slower → 1.1× faster); 10 rows 39 → 10 µs (8.7× → 2.9× slower); full
  scan + COUNT with filter 1.18 ms → 364 µs (2.5× slower → 1.3×
  faster); DB file size 327.7 → 294.9 KB (1.25× → 1.13× of SQLite);
  inserts, deletes and aggregates all now lead SQLite (see table above).
- Tests: 8 new cases in `persist_tests` — bare/projection/parameter/
  conjunct fast-path shapes, degenerate and out-of-bounds ranges,
  residual fallback, holes after deletes, multi-page spans, and the
  reordered/duplicate projection regression matrix across rowid-point,
  rowid-range and index-point fast paths.

## 6c. What was closed this sprint (2026-08-29, sixth pass): feature parity

- **FOREIGN KEY enforcement** (the largest remaining correctness gap):
  `PRAGMA foreign_keys = ON/OFF` toggles it (default off, SQLite's
  default). Child side: INSERT/UPDATE reject orphan keys (MATCH SIMPLE
  NULL pass-through; rowid-alias parent keys use O(log N) point probes).
  Parent side: DELETE applies ON DELETE RESTRICT / CASCADE (recursive,
  with index maintenance) / SET NULL / SET DEFAULT. Composite keys,
  implicit-PK (`REFERENCES parent`) refs, and text parent keys all
  enforced. 14 tests in tests/foreign_keys.rs.
- **DELETE on tables without INTEGER PRIMARY KEY**: the new
  `try_streaming_delete` walks the B+tree directly (rowid from the cell
  key, not a row column), fixing the long-standing "unsupported" error
  and removing per-row materialization for Scan/Filter/RowidRange/
  IndexRange sources in the same stroke.
- **JSON1**: a self-contained JSON engine (parser with unicode escapes
  and surrogate pairs, `$.a.b[0]` / `$[#-n]` path language, minifying
  serializer) backing `json`, `json_extract`, `json_valid`, `json_type`,
  `json_quote`, `json_array`, `json_object`, `json_array_length`,
  `json_insert`, `json_replace`, `json_set`, `json_remove`, `json_patch`
  (RFC 7396). Previously these silently returned NULL. 6 tests in
  tests/json1.rs.
- **ALTER TABLE**: RENAME TO (catalog entry move preserving index/trigger
  attachment, schema-row + `ON <table>` SQL rewrite, other tables'
  REFERENCES clauses rewritten, two-pass schema load so reordered rows
  resolve) and ADD COLUMN (SQLite restrictions enforced; DEFAULT
  physically back-filled into existing rows). RENAME COLUMN / DROP
  COLUMN parse and report a clean unsupported error. 11 tests in
  tests/alter_table.rs.
- **Dense page cache**: HashMap<PageId, PageRef> replaced by a
  direct-indexed Vec for page ids < 2^20 (4 GB file), HashMap overflow
  beyond — no hashing on any B+tree descent, bounded memory.
- Test count: 164 (120 lib + 11 alter + 14 fk + 6 json + 13 integration).

## 7. Working order (next sprint)

All head-to-head criteria are now at parity or faster (see §1). The
2026-08-31 leaf-cache close-out (f80c620) additionally landed: a
direct-mapped 64-slot table-leaf cache keyed by rowid (fixed-parameter
OLTP + join inner loops hit ~10/10 instead of 1/10 and skip the whole
root→interior→leaf descent), struct-local B+tree hints probed at ~2 ns
before the thread-local map, `BtreeHandleState` export/import for
cross-statement pinned roots, fused IndexLookup projection (selective
decode under the page lock, one B+tree handle per rowid batch), an
allocation-free `resolve_column_index`, and a like-for-like materializing
join harness (both engines now build the result rows; stepping without
reading values was comparing different work). Isolated steady-state:
2-table join 2.87 µs vs SQLite 2.78–2.95 (parity); 3-table join
20.3 µs vs 22.1 (**1.09× faster**).

### Remaining known deltas (all ≤5% or environmental)

- **GROUP BY 100 buckets** — 1.85 vs 1.84 ms (1.005×): sub-noise.
- **Mixed 80/20** — 2.54 vs 2.42 ms (1.05×): interleaved writes bump the
  write epoch and clear the leaf caches, forcing reads to re-descend.
- **2-table join in-bench** — 3.4 vs 2.8 µs in the full bench run, but
  isolated best-of-5 is 2.87 vs 2.78–2.95 µs (parity): the in-bench delta
  is CPU-cache pollution from the preceding sections on this shared
  2-core VM (we pay +30% because our advisory leaf-cache state is
  cache-resident, SQLite pays +5%), not engine work.
- **DB file size** — 1.03×: 8 KiB page rounding; `PRAGMA page_size=4096`
  matches SQLite exactly.
- **Binary size** — ~2.17 vs 2.03 MB: mimalloc adds ~140 KiB and buys the
  1.5–2.1× write-throughput wins.

The backlog below is hardening / depth work, ordered by user impact:

1. ⏳ Savepoint semantics — SAVEPOINT/ROLLBACK TO/RELEASE are no-ops on a
   flat transaction; implement nested savepoint snapshots.
2. ⏳ MVCC visibility wiring + connection pool — concurrent reads already
   win 8.3× (per-page locks vs a serialized connection), but concurrent
   WRITERS still serialize on the outer write lock.
3. ⏳ Differential fuzzer (`tests/fuzz_differential.rs`) — random SQL
   generators compared against SQLite, beyond the 206 hand-written cases.
4. ⏳ Collations: NOCASE / RTRIM indexes (BINARY is the only collation).
5. ⏳ UPDATE FROM (SQLite 3.33+), DELETE...RETURNING edge cases.
6. ⏳ Prepared-statement handle API (SQLite's sqlite3_step model) so
   callers can stream rows instead of materializing Vec<Row>.

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
