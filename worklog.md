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
