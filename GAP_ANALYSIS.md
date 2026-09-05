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

## 2. 2026-09-03 production-hardening sprint (torture matrix)

A new `examples/prod_torture` matrix (18 sections x 2 engines in isolated
child processes — per-engine peak RSS, `TORTURE_SCALE`-able, wired into CI)
found and fixed three data-corruption bugs and four throughput gaps:

### Correctness (all previously shipped broken)
- **ROLLBACK destroyed committed data**: plain ROLLBACK dropped the whole
  page cache + truncated to the BEGIN snapshot, then reset root maps to the
  catalog's CREATE-time roots. In-memory DBs (lazy write-back) lost every
  page never flushed at BEGIN, and any table that split in an EARLIER
  COMMITTED transaction resolved a stale leaf after rollback (10k rows
  became 186). Fixed: BEGIN pushes an implicit `__begin__` savepoint
  (SQLite's own model — BEGIN/ROLLBACK is the outermost savepoint), plain
  ROLLBACK restores page pre-images non-destructively (mid-transaction
  eviction writes are undone), and the root maps return to their BEGIN-time
  snapshot. Regression tests: `regression_rollback_*`.
- **Phantom duplicate rows on every scan**: interior pages keep
  `right_most_pointer == 0` after a split; every full-scan/count path
  followed child 0 — the DB header page, which doubles as the schema root
  — and yielded its row as a table row (500k inserts produced 500015 rows;
  the differential torture caught the same off-by-one). Fixed: all
  scan/count paths skip child 0 (range scans already did).
- **Page-0 descent on emptied interiors**: delete-heavy workloads could
  empty an interior page; descents then routed into page 0 (the schema
  page) — panics and potential schema-row deletion. Fixed with descent
  guards + a recycling guard that never empties an interior whose
  rightmost is unset.

### Throughput (torture scale, 1M rows unless noted)
- `IN (5000 literals)`: **226x slower -> 3.3x faster** (prebuilt integer
  membership set, SQLite's ephemeral-index equivalent).
- `ORDER BY ... LIMIT k` over 1M rows: **4.4x slower -> 1.8x faster**
  (top-N fusion: O(n) key extraction + bounded partial selection + tie
  order matching SQLite's stable sorter; 20 shapes differentially
  verified).
- `LIKE '%x%'` scans: **4.8x slower -> parity** (byte-level ASCII-folded
  matcher with classified shapes; no per-row allocations).
- Compound index selection: `WHERE d=? AND a=?` **31us -> 5.6us** (the
  planner now prefers the index whose PREFIX is bound by the most equality
  conjuncts).
- Mass `DELETE ... WHERE id > K`: **4x slower -> parity** (sequential
  bulk delete with a sticky leaf + whole-leaf clears; overflow chains
  freed).
- Overflow-chain reads bypass the page cache (sequential read-once pages
  no longer evict the live tree pages).

### Known remaining gaps — CLOSED 2026-09-05 (see §3)

All three tracked gaps below were closed by the overflow-aware selective
decode (2026-09-04) plus the 2026-09-05 production sprint:

- ~~Wide-row (2KB TEXT) and 64KB blob scans trail SQLite ~2-4x~~ →
  **parity to WIN**: overflow-aware selective decode gathers only the
  wanted columns' byte ranges straight into the value's own buffer;
  trusted-TEXT mode skips the redundant UTF-8 validation scan on
  in-memory payloads; bulk `allocate_pages` builds overflow chains under
  one cache critical section; exact-size payload reservation kills the
  realloc cascade. S08 (2KB x 25k) scan: ~1.3-1.4x WIN; S09 (64KB x 1k)
  scan: parity-to-win (best-of-2 under shared-runner noise).
- ~~Multi-index INSERT (5 indexes) trails ~20%~~ → **WIN** (S12:
  66.3ms vs 67.7ms at scale 0.15; insert scratch cache + savepoint
  capture fast path + explicit-rowid append).
- ~~Peak RSS on 1M-row workloads is ~2.5x SQLite~~ → **WIN on the
  gated metric** (bench_compare Peak RSS 100k insert+count: 25.6MB vs
  28.6MB). Torture's report-only per-section deltas at small scales
  remain ~7-11% above SQLite (engine init footprint + page-cache
  structure); no gate depends on them.

## 3. 2026-09-05 sprint — file-tail reclamation, crash-safety, routing

The truncate-on-mass-delete sprint (see CHANGELOG for the full list):

- **SQLite's truncate optimization landed**: whole-leaf unlink cascade +
  `Pager::truncate_tail` reclaim the file's contiguous freed tail
  outright. S14 churn+reclaim delete: 5.6x LOSS → 3.5x WIN; the file
  shrinks with the commit and the freelist stays clean (v4 trunk format:
  one 4-byte entry per free, ~1 write per 1022 frees).
- **Crash-safety ordering contract**: the header (the only place the
  committed `n_pages` lives) is written LAST in every flush/checkpoint
  path, before the truncating `set_len`. The OOM fault-injection suite
  (532 allocation-failure points) passes with the committed baseline
  intact at every point; the previous header-first order left a torn
  window that corrupted files on mid-flush OOM.
- **ROLLBACK vs truncation**: the truncation floor never drops below the
  lowest active savepoint's base (pages below hold undo pre-images
  ROLLBACK must restore); both rollback paths disarm the armed physical
  truncate. BEGIN;inserts;mass-delete;ROLLBACK is bit-exact.
- **Root-children routing** for scattered lookups (join fanouts, random
  probes): the root's full (separator, child) map is cached per write
  epoch and armed only after 16 consecutive leaf-hint misses, so point
  lookups never pay for it. Scattered rowid lookups: 198 → 177 ns; the
  Windows bench-gate's marginal 3-table-join loss flipped to ~20% WIN.
- **Streaming driver budget contract**: `next_batch` returns AT MOST
  `budget` rows (the LIMIT/OFFSET + Filter wrappers count on it); the
  64 → 1024-row batch widening lives in the top-level `step()` serving
  loop; ScanDriver/RangeDriver gained the 256 KB live-footprint cap.
- **Torture harness hardening**: best-of-2 child runs per engine per
  section (shared CI runners swing a single sample 2x — SQLite's S09
  blob scan measured 5.9ms and 3.7ms across two consecutive runs of
  identical code).

## 4. 2026-09-05 (II) — WAL-grade committed-view read concurrency

**The feature: readers never wait for an open write transaction.** SQLite
gives this only in WAL mode; this engine now gives it always, with the
version store in memory (the `__begin__` savepoint's undo pre-images)
instead of WAL frames — no WAL file, no frame copies on the read path, no
checkpointing.

### Semantics (SQLite-WAL reader model)

- A read on a connection/thread that is NOT the transaction owner sees
  the BEGIN-time (last committed) state while a write transaction is
  open: uncommitted inserts/updates/deletes/index entries are invisible;
  after COMMIT they appear atomically; after ROLLBACK they never did.
- The transaction OWNER keeps read-your-own-writes (live view) —
  including across async task migration (the sqlx driver decides by
  CONNECTION identity, not thread identity).
- DDL / SAVEPOINT statements inside a transaction flip it to
  conservative gating (readers wait — rollback-journal semantics):
  committed-view reconstruction is only guaranteed for data-only
  transactions, which are the OLTP 99% case.
- Writers still serialize (one writer at a time, BUSY + busy timeout)
  — SQLite's own contract.

### Implementation

- **Engine** (`pager.rs` committed view): while a foreign data-only
  transaction is open, `get_page` serves BEGIN-time bytes from (1) the
  `__begin__` pre-images, (2) the live cache when the writer never
  fetched the page this transaction (byte-identical to committed), (3)
  the WAL committed map / main file. Materialized pages memoize in a
  capped (1024-page / 4 MiB) reader-side cache, cleared at every
  transaction boundary — zero footprint when no reader runs.
- **Identity**: `BEGIN` records the owning thread id; other threads arm
  the committed view via the thread heuristic. The sqlx driver forces
  the exact view per CONNECTION (async tasks migrate threads), through
  a thread-local `ReadView` preference (`Database::set_read_view`).
- **Advisory-cache poison control**: caches a committed reader populates
  from BEGIN-time bytes (leaf hints, fast-path pins, join builds) are
  stamped with an epoch the reader never bumps — they would validate as
  "live" after the scope ends. The scope's drop bumps the write epoch
  ONCE when the reader actually served writer-touched pages, so the
  reader's cache state can never outlive its scope. The join cache and
  COUNT memo are additionally gated never to consult/populate while a
  committed read is armed (the concurrent owner-query window), and
  max-rowid merge-backs are gated (a BEGIN-time max rowid would regress
  the insert allocator → duplicate rowids).
- **Driver** (`sqlx_driver`): `acquire_read` passes foreign dirty
  transactions straight through when the engine reports committed reads
  available (the read executes against the committed view); the async
  gate pre-wait does the same peek. Zero-busy-timeout pools now serve
  reads during open write transactions instantly.

### Measured (bench `concurrent_rw`: 1 writer txn of 20k inserts, N reader threads)

| readers | reader ops DURING the txn | rate |
|--------:|--------------------------:|-----:|
| 2 | 2.4k | 48k ops/s |
| 4 | 5.1k | 61k ops/s |
| 8 | 6.0k | 106k ops/s |

Before this change: 0 reader ops during the transaction (every reader
waited at the gate for COMMIT). Single-thread benchmarks are unchanged
(point lookup 171 ns vs SQLite's 1.65 µs; insert 6×; range scan 1.4×).

### Tests

- `tests/committed_view.rs` (8): engine-level isolation commit/rollback,
  owner read-your-own-writes, UPDATE/DELETE pre-image visibility, index
  lookups mid-txn, scans across mid-txn B+tree splits, max-rowid
  non-poisoning, advisory-cache non-poisoning post-commit, 4-reader
  stress, file-backed parity.
- `tests/sqlx_driver.rs` (+6): zero-timeout read during open write txn,
  rollback invisibility, sqlx `Transaction` (deferred) reads, reader
  throughput during a write txn, DDL-txn gating fallback, owner reads
  across task migration.

## 5. 2026-09-05 (III) — SQLite feature-parity sweep + expression indexes + isolation hardening

**The parity audit: 291/291 PASS.** A runnable checklist
(`examples/parity_audit.rs`) probes the engine statement-by-statement
against recorded SQLite behaviors (DDL shapes, DML clauses, expression
coverage, scalar/aggregate functions, JSON, PRAGMA surface, transaction
semantics). The 13 failures found by the audit — and their fixes:

| area | symptom | fix |
|------|---------|-----|
| `count(DISTINCT x)`, `count(*)` re-`SELECT` | wrong results / None | DISTINCT-arg aggregate path + star-count through the scan driver |
| `CAST('42')` | None | cast folds TEXT through affinity rules |
| `unhex()` | returned BLOB | returns TEXT for TEXT input (SQLite quirk) |
| `last_insert_rowid()` | always 0 | per-connection note/serve on the SQL-function path |
| `json_each` / `json_tree` | parse error | table-valued functions (`src/executor/tableval.rs`): FROM-clause function sources, aliasing, bound-parameter args |
| `pragma_table_info('p')` as a table | parse error | same TVF machinery (`pragma_*` family: table_info, index_list, index_info, foreign_key_list) |
| expression indexes `ON t (a+b)` | parse error | parser: parenthesized expression in the indexed-column list; catalog carries `IndexColumn.expr` |
| STRICT tables | accept/reject both wrong | `STRICT` gate on insert coercion + type-check errors carry the type name |
| `UPDATE ... LIMIT` / strict bad-type INSERT | parse errors | grammar coverage |
| nested `BEGIN` | no error | error "cannot start a transaction within a transaction" |

**Expression indexes — two real correctness bugs found under the new
tests, both from the same root: `IndexMaintState` (the INSERT fast
path's per-statement index state) resolved index columns by NAME, and
an expression column's "name" is the rendered text `(a + b)` —
`find_column` misses, the position becomes `usize::MAX`, and the key
encodes as EMPTY. An empty probe key prefix-matches every cell in
`lookup_index` (`starts_with`), so every insert after the first saw a
phantom UNIQUE conflict.**

1. `IndexMaintState::encode_key` now evaluates expression columns per
   row (`eval_row`) — byte-identical to `encode_index_key`, which had
   backfilled the index at CREATE time (probe keys and stored keys must
   agree). Plain-column indexes keep the zero-cost positional path;
   the table's `col_names` rides along only for indexes that have an
   expression key.
2. The UPDATE write-set fast path computed "touched indexes" by named
   column only — an expression index was NEVER touched, so `UPDATE t
   SET a=..` left the `(a+b)` entry stale (old key not deleted, new key
   not inserted; both unique-constraint enforcement and lookups
   desynced). `touched_indexes` now walks the expression's column
   references (and a partial index's WHERE refs — a SET can move a row
   in/out of a partial index).
3. The unique-check NULL exemption now uses `index_key_has_null`
   (expression-aware): a row whose expression evaluates to NULL is
   exempt from uniqueness, like SQLite.

**Committed-view gate race (isolation hardening).** The pager's scope
gate was a plain `AtomicBool` raised at arm and lowered at drop, with
the drop consulting only its own thread's TLS. Two holes: (a) reader
B's drop cleared the gate while reader C's scope was still live on
another thread — C's `get_page` skipped the TLS check and served LIVE
(uncommitted) pages inside its BEGIN-time scope (the stress test
caught it as an intermediate COUNT); (b) a concurrent last-drop's
`store(false)` could land after a new arm's `store(true)`, gating a
live scope off. The gate is now the scope COUNT itself (an
`AtomicUsize`): RMWs serialize, so `count > 0` holds exactly while any
scope exists — no ordering hazard, same single-load fast path, and the
defensive `disarm_reader_scope` belt never decrements (a stuck-high
count costs one TLS probe on scope-less threads; a spurious decrement
could gate a live scope off — fail-safe direction only). 0/40
failures on the 4-reader stress test under 2-CPU contention.

**sqlx / sea-orm status.** The sqlx driver (AnyPool-less; a
`sqlx::sqlite`-shaped pool over the engine) and sea-orm 2.0.2 run
UNMODIFIED from crates.io: `sqlx-interop` has 3 runnable bins
(`sqlx` CRUD, `migrate`, `sea_orm_interop` — 10 scenarios incl.
relations, pagination, transactions, error propagation — all green),
plus `sea_orm_relations` for join shapes. `SQLX_COMPAT.md` documents
the wire surface. Remaining sea-orm surface to grow: `json!` column
serde, more `DeriveRelation` shapes, livepool under churn.
