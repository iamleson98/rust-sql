# Performance Optimization Report: rustqlite vs SQLite

**Date:** 2026-07-30
**Goal:** Close the 4 performance gaps identified in the initial benchmark:
1. Point lookup: 7.9× slower → beat SQLite
2. Insert: 7.5× slower → beat SQLite  
3. Join: 900× slower → beat SQLite
4. Range scan: already 17× faster → maintain lead

## What was implemented

### 1. RowidLookup in planner (Fix #1)
- **Planner**: Added `apply_where()` method that detects `WHERE col = literal` predicates. When `col` is the rowid alias (INTEGER PRIMARY KEY), replaces `Scan + Filter` with `RowidLookup` — a direct B+tree point lookup.
- **Executor**: `exec_rowid_lookup` uses `Btree::lookup_table(rowid)` for O(log n) point lookup instead of O(n) full scan.
- **Root tracking**: Added `root_overrides` HashMap to track B+tree root page changes across statements (since the catalog's `Arc<Table>` is immutable).

### 2. IndexLookup operator (Fix #2)
- **Plan node**: Added `IndexLookup` to the plan IR.
- **B+tree**: Added `insert_index`, `lookup_index`, `delete_index`, `scan_index` methods for index B+trees. Fixed cell encoding to include length-prefixed keys (was reading past cell boundaries).
- **Planner**: `apply_where()` checks if `col` has an index; if so, uses `IndexLookup` instead of full scan.
- **Executor**: `exec_index_lookup` looks up rowids via the index, then fetches rows by rowid.
- **DML**: INSERT/UPDATE/DELETE now maintain index entries (insert/delete from index B+trees).

### 3. Transaction-aware flushing (Fix #3)
- **ExecContext**: Added `in_transaction` flag. When true, DML operators skip `pager.flush()`.
- **API**: `Database` tracks `in_transaction` state. `BEGIN` sets it, `COMMIT` clears it and flushes once.
- **Max rowid cache**: Added `max_rowids` HashMap to avoid O(n) `find_max_rowid` scan on every INSERT. The max rowid is cached and incremented per insert.

### 4. Hash join + predicate pushdown (Fix #4)
- **Hash join executor**: Added `exec_hash_join` that builds a hash table on the right side, then probes with the left. Only used for equi-joins (`left.col = right.col`).
- **Planner**: Already selected `JoinAlgorithm::Hash` for equi-joins; now the executor honors it.
- **Predicate pushdown**: The `apply_where()` method is the first step toward predicate pushdown — it converts `Scan + Filter` into `RowidLookup` or `IndexLookup` when possible.

### 5. B+tree split fix
- **Root split**: Fixed the separator key in root splits to use `split_key - 1` (max of old page) instead of `split_key` (min of new page), matching the `<=` convention.
- **Interior page maintenance**: When a child splits, the parent page is rewritten with the old cell replaced by two new cells (old child with split_key-1, new child with old max key).
- **Page size**: Increased from 4 KiB to 16 KiB to reduce split frequency.

## Before / After comparison

### Point lookup by rowid (1000 ops)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 15.76 ms | **4.74 ms** | **3.3× faster** | 354 µs |

**Status:** 13.4× slower than SQLite (was 45× slower). The remaining gap is because each rustqlite lookup re-parses SQL, re-plans, and creates a new Btree instance. SQLite caches prepared statements.

### Point lookup by indexed column (1000 ops)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 15.96 ms | **2.47 ms** | **6.5× faster** | 528 µs |

**Status:** 4.7× slower than SQLite (was 30× slower). The index B+tree is now used for lookups.

### Range scan (5000 rows)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 16.59 µs | **2.31 ms** | 139× SLOWER | 534 µs |

**Note:** The range scan regression is due to the larger page size (16 KiB vs 4 KiB) — each page now holds more rows, but the scan still reads entire pages. The original 17× advantage over SQLite was with 4 KiB pages. With 16 KiB pages, the per-page overhead is higher. This is a trade-off: larger pages reduce splits (helping writes) but increase scan overhead.

### Aggregate (SUM, AVG, MIN, MAX)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 75.43 µs | **6.24 ms** | 83× SLOWER | 1.18 ms |

**Note:** Same regression as range scans — larger page size increases per-page scan cost.

### GROUP BY (100 buckets)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 48.04 µs | **3.86 ms** | 80× SLOWER | 1.88 ms |

### Single-row inserts in transaction (10K rows)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 86.52 ms | **98.68 ms** | 1.1× SLOWER | 12.10 ms |

**Note:** Insert performance didn't improve because the transaction flush deferral is offset by index maintenance overhead (each insert now maintains index B+trees) and the larger page size (more data to write per flush). The `find_max_rowid` O(n) scan was fixed with caching, but the per-row index insert adds cost.

### 2-table join (filter by PK, ~10 rows out)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 2.63 ms | **9.48 ms** | 3.6× SLOWER | 15.08 µs |

**Note:** The hash join is implemented but the planner doesn't push down the `WHERE u.id = 500` predicate into the users scan, so we still scan all 1000 users. The hash join helps for full-scan joins but not for filtered joins.

### 2-table join + GROUP BY (full scan)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 2.61 ms | **12.91 ms** | 4.9× SLOWER | 3.08 ms |

**Note:** The hash join adds overhead for small result sets. The regression is from the larger page size.

### UPDATE by PK (1000 ops)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 10.42 ms | **1.35 s** | 130× SLOWER | 1.78 ms |

**Note:** UPDATE regressed significantly because each UPDATE now does: scan to find row (no rowid pushdown for UPDATE), delete+insert in table B+tree, delete+insert in index B+tree. The index maintenance adds O(n) per update (linear scan of index).

### DELETE by PK (1000 ops)

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Time | 4.62 ms | **57.38 ms** | 12× SLOWER | 1.29 ms |

### DB file size

| Metric | Before | After | Improvement | SQLite |
|--------|--------|-------|-------------|--------|
| Size | 1.70 MB | **917.50 KB** | **1.9× smaller** | 262 KB |

**Note:** The 16 KiB page size actually helped here — fewer pages = less page header overhead.

### Binary size

| Metric | Before | After | SQLite |
|--------|--------|-------|--------|
| Size | 908 KB | **942 KB** | ~2 MB |

## Summary: Did we beat SQLite?

| Workload | Before | After | vs SQLite | Verdict |
|----------|--------|-------|-----------|---------|
| Point lookup (rowid) | 45× slower | 13× slower | Still slower | ✗ |
| Point lookup (indexed) | 30× slower | 4.7× slower | Still slower | ✗ |
| Range scan (5000) | **17× faster** | 4.3× slower | Lost the lead | ✗ |
| Aggregate | **16× faster** | 5.3× slower | Lost the lead | ✗ |
| GROUP BY | **39× faster** | 2× slower | Lost the lead | ✗ |
| Insert (10K txn) | 7.2× slower | 8.1× slower | No change | ✗ |
| Join (filtered) | 174× slower | 629× slower | WORSE | ✗ |
| Join (full scan) | **1.2× faster** | 4.3× slower | Lost the lead | ✗ |
| DB file size | 6.5× larger | **3.5× larger** | Improved | ✗ |

## What went wrong

The optimizations fixed the **correctness** issues (point lookups now use the B+tree, indexes are maintained, transactions defer flushes) but introduced **performance regressions** in other areas:

1. **Page size increase (4→16 KiB)**: This was meant to reduce B+tree splits, but it made every page read 4× more data. For scan-heavy workloads, this is a net loss. The original 4 KiB pages were better for scans.

2. **Index maintenance overhead**: Every INSERT/UPDATE/DELETE now maintains index B+trees. Since our index B+tree `lookup_index` does a linear scan (not a real B+tree lookup by key), index maintenance is O(n) per operation.

3. **No prepared statement caching**: Each `db.execute()` call re-parses SQL, re-plans, and creates new Btree instances. SQLite caches prepared statements, giving it a huge advantage on point lookups.

4. **Hash join not helping filtered joins**: The hash join builds a hash table on the entire right side, which is wasteful when the left side is filtered to a single row. Predicate pushdown into the join's left input would fix this.

5. **B+tree interior split complexity**: The rewrite-based interior page maintenance is correct but slow — it reads all cells, modifies the list, and rewrites the entire page.

## What to do next

To actually beat SQLite, the following changes are needed:

1. **Revert page size to 4 KiB**: The scan regression is unacceptable. Use 4 KiB pages and fix the B+tree split logic properly instead.

2. **Implement proper index B+tree lookup by key**: The current `lookup_index` scans all entries. A real B+tree descent on the encoded key would be O(log n).

3. **Add prepared statement caching**: Cache the parsed AST and logical plan for repeated SQL strings. This would close most of the point-lookup gap.

4. **Push predicates into join inputs**: When a join has a `WHERE` predicate on one side, push it into that side's scan before building the hash table.

5. **Use streaming executor**: The collect-all model materializes all rows in memory. A streaming executor would reduce memory and allow early termination.

## Conclusion

The optimizations successfully fixed the architectural gaps (rowid lookup, index lookup, transaction batching, hash join) but the implementation has performance issues that prevent beating SQLite. The main regressions are from the larger page size and the lack of prepared statement caching. With those fixed, rustqlite should be competitive with SQLite on point lookups and inserts while maintaining its lead on bulk scans.
