# rustqlite worklog

---
Task ID: 1
Agent: main
Task: Close all documented gaps vs SQLite; make rustqlite beat it (continued session)

Work Log:
- Rebuilt context: baseline all tests green (650+); bench_compare baseline captured (UPDATE by PK 1.03x, S06 0.92x tracked LOSS).
- GAP: UPDATE by PK — diagnosed payload-size flips (codec v2: integral REALs = 2-5B zigzag, fractional = 9B double) defeating update_table's same-size patch → full delete+insert.
- FIX: in-leaf shape-changing replace — `Btree::apply_leaf_replace`: SHRINK branch writes new cell into the old cell's slot (no offset moves, hints stay valid via note_write_in_place); GROW branch rebuilds at content start when free space allows (note_write). Wired into update_table's hint probe + descent, both fallbacks (single-row fast path, collect path, FK cascade updates).
- RESULT: UPDATE by PK 1.74ms → 1.26ms = 1.03x → 1.51x vs SQLite. tests/replace_cell.rs (8 tests): grow/shrink roundtrips, index consistency, churn-persistence, overflow fallback, triggers, split compaction.
- GAP: S06 streaming step path per-row Row allocation.
- FIX: row pool — Driver::next_batch takes `pool: &mut Vec<Row>`; btree `scan_table_range_selective_pooled` pops cleared row buffers; Statement recycles served rows (current_row/pending at step/reset/EOF). Drivers pooled: ProjectedScan/ProjectedRange (pooled selective scan), Scan/Range (decode_row_into), FilteredScan (6 sites), GroupBy finalize, Project outputs; Filter/Limit forward pool.
- RETENTION BUG FOUND+FIXED: pool retained blob payload bytes (S09 72→119MB, scan 0.38x) — clear-on-return keeps only slot capacity. S09 back to 1.01x TIE; S06 now 1.43-1.54x WIN.
- Full test matrix re-verified green after each change.

Stage Summary:
- UPDATE by PK: 1.03x → 1.51x (beats SQLite clearly now)
- S06 range scan step path: 0.92x LOSS → 1.43x+ WIN
- New: tests/replace_cell.rs; btree.rs apply_leaf_replace + scan_table_range_selective_pooled; statement.rs row_pool.
- Remaining gaps: parallel join (multi-table world), index overflow interop, preupdate hooks, CAST/ORDER-BY on non-UTF-8 files, ptrmap write.

---
Task ID: 2
Agent: main
Task: Verify parallel-join WIP, close the join-correlated regression, update README, push

Work Log:
- Rebuilt context after session outage; parallel-join probe (try_parallel_join_probe in executor/parallel.rs + mod.rs wiring) built clean; tests/parallel_join.rs 5/5 green.
- Probe benchmark (this host, 2 cores): 1M x 1M join, 3M out — serial 468.4 ms, parallel 451.6 ms, SQLite 624.2 ms => 1.38x vs SQLite. Parallel split only 1.04x (2-core box; build side serial).
- Full matrix run exposed a REGRESSION: correlated_with_join_context FAILED — the new no-Project fused-join path synthesized combined rows with UNQUALIFIED output names ("id" not "u.id"/"o.id"), so downstream correlated resolution of `o.id` bound to the FIRST same-named column (users.id) instead of orders.id.
- FIX: synthesized ProjectExprs now carry alias: Some(qualified name) so the join's output columns are byte-identical to the materialized path's combined columns ("u.id", "o.id", ...). Correlated 7/7 green after fix.
- Full matrix re-verified: 677 passed / 0 failed across 48 test binaries (release, fat-LTO). cargo fmt applied (new example/test files) and fmt --check clean; release build warning-free (unused_mut removed in btree.rs).
- README updated to current state: 675+ tests (was 650+), UPDATE-by-PK table row 1.26 ms vs 1.90 ms (1.51x, post in-leaf replace), new 1M-row parallel table join row (1.38x), "Where the wins come from" parallel-join bullet, gap ledger rewritten (parallel executor coverage now includes join probe split; "two narrow serial rows" -> one).

Stage Summary:
- Parallel JOIN probe: verified, bit-identical to serial, 1.38x vs SQLite at 1M scale.
- Join-context correlated-subquery regression: found by full-matrix verification, fixed by qualified synthesized column names.
- 677/677 green; fmt + release build clean; README current; ready to commit + push.
- Remaining gaps (unchanged): preupdate hooks, index-overflow interop, non-BINARY collation write, ptrmap write, CAST/ORDER-BY on non-UTF-8 files, WAL sidecar for SQLite-format mode.
