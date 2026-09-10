
---
Task ID: 5
Agent: main (Super Z)
Task: Rebuilt workspace after reset; verify HEAD; close the non-BINARY collation gap (write side + engine semantics)

Work Log:
- Environment was wiped mid-session: re-cloned iamleson98/rust-sql from GitHub (token from the recovered CI poll script), reinstalled rust stable 1.98.1 + clippy + rustfmt. Remote state: b1ed5b6 CI FAILED (4x clippy lints in probe_par_join + macOS torture S08 jitter under the 2.0ms floor) — ALREADY fixed by 31ca53e (CI green), which also shipped the index-overflow gap closure 2b50cea (CI green). Dev matrix at HEAD: 683/683.
- Probe (examples/probe_coll_write.rs, real SQLite judges the written file): NOCASE write side was already correct for explicit/inherited/autoindex/DESC indexes (integrity_check ok) — but WITHOUT ROWID NOCASE PKs wrote rows in scan order ("row not in PRIMARY KEY order"). FIX: build_bytes now passes pk_collations to build_record_tree for every non-BINARY-PK / non-UTF-8 WithoutRowid (was: UTF-8 always skipped the sort, trusting the caller's order).
- Probe also exposed ENGINE-side collation bugs, all fixed:
  1. ORDER BY on a column with a DECLARED collation sorted BINARY (resolve_order_by_terms now attaches the term's collation — declared for bare-column terms, explicit COLLATE read from the ORIGINAL term so the aggregate rewrite can't lose it).
  2. Index selection matched columns by NAME only: a BINARY equality could seek a NOCASE index (wrong rows). Added index_column_serves_conjunct gates in apply_where_for_scan (first + prefix columns), try_index_in, try_index_range; extractors (eq/range/between) now see through COLLATE-wrapped operands.
  3. The OLTP FastPaths (IndexPoint/IndexCount) did not fold probe keys — literal pre-encoding AND runtime parameter encoding now fold through the index columns' collations; uppercase probes over NOCASE indexes found 0 rows before.
  4. Project{Aggregate{IndexLookup}} FastPath lacked the COUNT-only gate: SUM/MIN/MAX/GROUP_CONCAT over an index equality returned the MATCH COUNT as their value (pre-existing correctness bug; gated now).
  5. GROUP BY under the term's collation: HashGrouper folds keys for hash/equality (spill codec + parallel merges stay consistent), keeps each group's FIRST-SEEN ORIGINAL as the display/representative (SQLite's output); wired in scan_groupby_grouper (serial + both parallel paths + merge groupers) and the generic path (collect_plan_tables alias-aware scope walk); RamTail/finish emit the representative.
  6. rewrite_aggregates_and_groups now matches projections/ORDER BY terms to GROUP BY terms through an explicit COLLATE wrapper (was NULL output columns).
  7. UPDATE/DELETE planning never passed through rewrite_column_collations — declared-collation WHERE clauses compared BINARY (and the new gates declined the matching index). plan_update/plan_delete now rewrite the predicate with the target table's scope first.
- tests/collate_semantics.rs (19 differential suites vs bundled SQLite): ORDER BY (declared/explicit/alias/ordinal/DESC/RTRIM), index probes (equality/IN/range/BETWEEN, both mismatch directions), fast-path folding (literals + parameters), GROUP BY (declared, explicit-COLLATE, multi-term mixed, join-qualified, filter+HAVING, RTRIM, first-seen representative both orders), UPDATE/DELETE through NOCASE indexes, 300k-row parallel collated GROUP BY serial-equality.
- Verification: dev matrix 702/702, release matrix 702/702, fmt clean, clippy -D warnings clean (default + no-default + sqlx). probe_coll_write: all 5 file-write shapes integrity-ok with SQLite re-opening.

Stage Summary:
- Collation ledger entry closed: NOCASE/RTRIM writes (explicit, column-declared, autoindex, WITHOUT ROWID PK, DESC) + engine-side collation semantics (ORDER BY, index gating, probe folding, GROUP BY incl. parallel/spill, DML WHERE). Remaining: custom collations still binary in written files (no portable encoding).
- 3 pre-existing correctness bugs found by the new differential suite: FastPath aggregate-value-as-count, UPDATE/DELETE collation-blind WHERE, NOCASE index probing with unfolded keys.
- 702 tests; ready to commit + push (README + worklog updated).

---
Task ID: 6
Agent: main (Super Z)
Task: Close the preupdate-hook C ABI gap (engine + compat) with SQLite-differential proof

Work Log:
- CI status on 3f9df95 (non-BINARY collation work): ALL GREEN (22/22 jobs incl. bench gates + torture on 3 OSes) — the earlier CI failures (clippy lints + torture jitter) were already fixed by 31ca53e/2b50cea.
- Oracle pinning (examples/preupdate_oracle.rs, rusqlite preupdate_hook + bundled SQLite): WITHOUT ROWID events fire with rowid=0; INSERT OR REPLACE = DELETE(old)+INSERT(new); upsert DO UPDATE = one UPDATE; rowid-changing UPDATE = ONE UPDATE; trigger bodies fire at depth 1, nested triggers at 2; FK actions fire at depth 1+, CASCADE chains increment per level, table order = REVERSE declaration; DDL fires nothing; defaults materialize into event values.
- Engine infrastructure: new src/preupdate.rs (PreupdateEvent {op, db, table, rowid, old, new, depth}, PreupdateOp::code() = 18/9/23, thread-local sink with Borrowed{mutex,db-id}/Owned variants, depth counter, unwind-safe guards). Database::set_preupdate_hook (Mutex slot; fire locks per event so shared-&Database statement stepping is race-free); execute() + statement::exec_with_ctx install the sink per statement; nested same-Database execute detected by sink identity (no mutex re-entry).
- Fire sites: exec_insert_one_row (append fast path, buffered plain, rowid-conflict REPLACE pair, UNIQUE-conflict REPLACE), exec_upsert_row, both UPDATE apply loops (alias-move = ONE event), process_update_row collect-time fires (patch + generic paths), single-row OLTP fast path, streaming DELETE per-row + RowidLookup fast path (parent event BEFORE FK actions), delete_rows_by_rowid (+ no-fire inner variant for internal alias moves), FK CASCADE/SET NULL child events, api.rs exec_chain_row fast insert. Fused in-place patch + bulk delete fast paths gated off when a hook is installed (need per-row old values).
- triggers.rs: recursion gate now SQLite's name-on-stack semantics (a trigger re-firing ITSELF is stopped; DIFFERENT chained triggers run at depth 2+).
- enforce_parent_delete_fks: self-referencing tables no longer excluded (tree-shaped schemas cascade, depth guard stops cycles); FK actions now apply in REVERSE declaration order (SQLite parity, catalog gained table_creation_seq); cascade recursion moved after the child's fire+delete (grandchild events follow the child's).
- Differential suite tests/preupdate_differential.rs: same battery through rusqlite + engine, event streams must be IDENTICAL — 41 events, now passing. Also pinned op codes + hook removal + WR rowid=0.
- Compat C ABI: sqlite3_preupdate_hook/count/depth/old/new with CORRECT signatures (sqlite3* handle, not stmt — the old stubs had them wrong), per-connection bridge (install_owned around the DML step path; Weak<Conn> capture; event stash + value pool freed at event end), SQLITE_MISUSE/SQLITE_RANGE error sides. 4 C-ABI tests in compat/rustqlite-compat/tests/preupdate.rs (event stream incl. FK reverse-decl order + trigger depth, accessor errors, step-path firing, WR rowid=0).
- Fix from the C-ABI tests: compat's scan_stmt_end was not trigger-body-aware — CREATE TRIGGER through sqlite3_exec mis-split at the body's internal ';' (port of split_script's CREATE/TRIGGER + BEGIN/END depth tracking).
- rusqlite preupdate_hook oracle needs libclang via upstream's preupdate_hook->buildtime_bindgen feature edge; vendored libsqlite3-sys 0.30.1 at vendor/ with that ONE edge removed (pre-generated bindings already declare the sqlite3_preupdate_* family; build.rs still adds -DSQLITE_ENABLE_PREUPDATE_HOOK) + [patch.crates-io] in the root workspace. No libclang dependency anywhere.
- Differential battery ALSO surfaced (not fixed, ledger'd): WITHOUT ROWID PK uniqueness is unenforced on the internal path (dup PKs accepted; ON CONFLICT (pk) errors) — needs an engine-internal unique index over the PK excluded from the file-format dump.
- Verification: fmt clean; clippy -D warnings clean in all 4 configs; dev matrix 704/704 (lib+integration) + compat 56/56 + doc 5/5; full-suite exit 0.

Stage Summary:
- Preupdate-hook family fully real on both the native API and the C ABI, with the event stream differentially proven against real SQLite (41 events).
- 4 engine/compat bugs found by the differential and fixed: nested-trigger suppression, self-referential FK cascade exclusion, compat trigger-DDL script splitting, FK action order (now reverse-declaration like SQLite).
- New gap documented: WITHOUT ROWID PK uniqueness (internal path).
- Ready to commit + push.

---
Task ID: 7
Agent: main (Super Z)
Task: Close the WITHOUT ROWID PK uniqueness gap (found by the preupdate differential)

Work Log:
- Gap: the engine stores WITHOUT ROWID tables as rowid tables internally; the PK had no index backing — duplicate PKs silently accepted, `ON CONFLICT (pk)` errored "does not match any PRIMARY KEY or UNIQUE constraint" (probe-confirmed before the fix).
- Schema: `IndexOrigin` enum (CreateIndex / Autoindex / WithoutRowidPk) on the catalog Index — provenance for the internal WR-PK index. New `without_rowid_pk_columns` helper (PK columns in pk_seq order, collation + order carried). `Catalog::rename_table` now also re-keys autoindex NAMES to the new table (pre-existing staleness fixed).
- CREATE TABLE: synthesizes `sqlite_autoindex_<t>_0` (the _0 slot — SQLite numbering starts at _1) over the PK columns, unique, origin WithoutRowidPk, NO schema row, no order-map entry. load_foreign_image / the sqlitefmt reader replay DDL through this path, so SQL-format opens get the index for free.
- Native reopen (load_schema): `rebuild_without_rowid_pk_index` — fresh root allocation + full row backfill (scan table, encode PK keys, insert), root moves tracked.
- ALTER RENAME COLUMN: the WR-PK index updates its catalog entry only (no schema-row delete/insert — it has none).
- Dump filter: `collect_foreign_dump` excludes origin WithoutRowidPk — SQLite files never carry a PK autoindex for WR tables (the table b-tree is the PK index there); real SQLite re-opens the dump and enforces the PK itself.
- Upsert target resolution now finds the PK index (name/column match) → `ON CONFLICT (pk)` works on WR tables; the preupdate differential's WR-upsert scenario restored (engine stream still SQLite-identical, 42 events).
- tests/without_rowid_pk.rs (3 suites, differential): single/composite PK duplicates with SQLite's exact messages, upsert/DO NOTHING/REPLACE/conflicting UPDATE, final-state equality, sqlite_master invisibility, PRAGMA index_list origin 'pk', native-format reopen backfill, SQLite-format dump round-trip (real SQLite opens + enforces; engine re-opens its own dump + enforces).
- Verification: full dev matrix 707/707, compat 56/56, fmt clean, clippy -D warnings clean in all 4 configs.

Stage Summary:
- WR PK ledger entry closed with file-format interop proven in both directions.
- Preupdate CI (250ed29): 20/21 jobs green at last poll (only windows test in flight).
- Ready to commit + push; remaining ledger: ptrmap writes, non-UTF-8 CAST/ORDER BY byte order, WAL sidecar writes for SQLite-format mode.
