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
