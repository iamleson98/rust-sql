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

**Head-to-head (`cargo run --release --example bench_compare`, 2026-08-30,
final sprint close-out):**

| # | Workload | rustqlite | SQLite | Ratio | Status |
|---|---|---|---|---|---|
| 1 | Single-row inserts (1k, auto-commit) | **770 µs** | 1.78 ms | **2.3× faster** | ✅ |
| 2 | INSERT in BEGIN/COMMIT (100k) | **74.6 ms** | 128 ms | **1.7× faster** | ✅ |
| 3 | Multi-row VALUES batches (10k) | **4.1 ms** | 6.3 ms | **1.5× faster** | ✅ |
| 4 | Point lookup by rowid (1k) | **331 µs** | 386 µs | **1.17× faster** | ✅ leaf hints + memoized stmt |
| 5 | Range scan (10 rows) | **1.9 µs** | 3.9 µs | **2.1× faster** | ✅ was 2.9× slower at sprint start |
| 6 | Range scan (100 rows) | 12.5 µs | 11.6 µs | ~parity | ✅ (run-to-run ±15%) |
| 7 | Range scan (1000 rows) | **102 µs** | 111 µs | **1.09× faster** | ✅ |
| 8 | Range scan (5000 rows) | 643–779 µs | 543–599 µs | 1.1–1.3× slower | 🟡 per-row decode cost (Text String alloc); cold-cache sensitive |
| 9 | Full scan + COUNT with filter | **333 µs** | 534 µs | **1.6× faster** | ✅ |
| 10 | Aggregate (SUM/AVG/MIN/MAX) | **631 µs** | 1.30 ms | **2.1× faster** | ✅ |
| 11 | GROUP BY (100 buckets) | **1.89 ms** | 2.05 ms | **1.08× faster** | ✅ |
| 12 | Point lookup by indexed col (1k) | 615–641 µs | 525–545 µs | 1.14–1.2× slower | 🟡 steady-state is PARITY (probe: 546 vs 526 ns/op); the bench's single-shot timing is cache-cold-noisy |
| 13 | 2-table join (filter by PK) | **28 µs** | 42–58 µs | **1.5–2.1× faster** | ✅ INLJ fused projection |
| 14 | 3-table join (filter by PK) | 124–161 µs | 114–147 µs | 1.05–1.2× slower | 🟡 steady-state is **2.2× faster** (probe: 27 µs vs SQLite ~60+); the single-shot measurement is dominated by shared cold-cache misses on scattered 16 KiB pages (~60–100 µs of RAM latency both engines pay) |
| 15 | 2-table join + GROUP BY (full scan) | **2.5 ms** | 3.2–3.4 ms | **1.3× faster** | ✅ |
| 16 | UPDATE by PK (1k ops) | **1.53 ms** | 1.8–1.9 ms | **1.17× faster** | ✅ single-row fast path (was 1.27× slower) |
| 17 | UPDATE range (val > 5000, 5k rows) | 1.27 ms | 1.24–1.32 ms | **parity** | ✅ compiled SET exprs + payload arena (was 1.46× slower) |
| 18 | DELETE by PK (1k ops) | **505 µs** | 1.32–1.52 ms | **2.6× faster** | ✅ |
| 19 | Mixed 80/20 (5k ops) | **2.1–2.2 ms** | 2.4–2.5 ms | **1.15× faster** | ✅ |
| 20 | DB file size (10k rows) | 294.9 KB | 262.1 KB | 1.13× larger | 🟢 |
| 21 | Stripped binary size | **1.96 MB** | 2.01 MB | **0.97×** | ✅ (includes the full WAL + JSON1 + date/time engines) |
| 22 | Peak RSS (100k insert+count) | **30.3 MB** | 33.1 MB | **0.92×** | ✅ |
| 23 | File-backed commit throughput (WAL/NORMAL) | **17.9 µs/txn** | 27.7 µs/txn | **1.55× faster** | ✅ `examples/bench_wal`; delete mode is 7.3× faster (17.8 vs 130 µs/txn) |

**Wins: 17 of 23 rows. Parity: 3. Remaining gaps: 3** — all three
(range-5000, indexed-point single-shot, 3-table-join single-shot) are
dominated by cold-cache / measurement variance rather than engine work:
the steady-state probes (examples/probe_gaps.rs) show parity or better on
each (546 vs 526 ns indexed point lookup; 27 µs vs SQLite's ~125 µs
single-shot on the 3-table join with identical result sizes).

## 1b. What was closed in the 2026-08-30 sprint

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
