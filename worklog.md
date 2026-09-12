
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

---
Task ID: 8
Agent: main (Super Z)
Task: Close the join-reordering gap (planner plan shapes) — greedy cardinality-driven INNER-join spine reordering + WHERE-conjunct fusion into join conditions

Work Log:
- Workspace wiped again (sandbox reset): re-cloned from GitHub (token via API), reinstalled rust 1.98.1 + clippy + rustfmt, re-fetched deps. Remote state: master 5892d4c, CI GREEN on HEAD (the earlier CI failure on a738eb6 was already fixed by 31ca53e/2b50cea lineage — nothing to push, all ledger items 1-6 of the original gap list closed through 5892d4c).
- Chose the ledger's own "Planner plan shapes" entry as the next highest-value item: joins were planned in syntactic left-deep FROM order.
- NEW `reorder_inner_joins` (planner): flattens maximal INNER/CROSS join spines of 3+ relation atoms (2-atom spines untouched — the executor's hash join already picks its build side, INLJ tries both directions); per-atom cardinality estimates from the pushed access path + sqlite_stat1 (RowidLookup=1, RowidIn=n, literal rowid-range span, IndexLookup stat1-est or 10, IndexIn members×est, Filter SQLite-style blind selectivities 1/10 / 1/3 / k/10, Scan stat1 rows or 2^20 unanalyzed default, CteRows len); greedy smallest-first with connectivity preference (disconnected picks = true cartesian regions); conditions re-attach to the FIRST join node covering their relations; column order restored with a bare-column projection (unique-names gate; hidden rowid slots exempt); subquery atoms / non-static names abort safely; outer joins PIN their subtrees (their own inner sub-spines still reorder).
- `pushdown_filter` (Join arm): INNER/CROSS cross-side WHERE conjuncts now FUSE into the join's ON condition (`FROM a, b WHERE a.x = b.x` == `... JOIN b ON a.x = b.x`) — implicit-join syntax was executing as a cross product + post-filter before; with the fusion + Hash algorithm it hash-joins. Outer joins keep every predicate on top.
- Multi-equi ON conditions (`ON a.x = b.x AND a.y = b.y`) now plan as Hash (condition_has_equi_leaf) — they previously took the NestedLoop path and never hashed.
- Executor `col_index`: qualified references now fall back to PLAIN-NAME exact matching (no suffix) — a pushed RowidLookup/IndexLookup side reports unqualified columns, so `a.x = b.id` extracts its key pair instead of degrading to a per-pair nested loop.
- THREE binding bugs found and fixed during the differential bring-up (all in the new code):
  1. The driver's Join arm consumed the spine WITHOUT the top filter, hiding it behind the restoration Project so WHERE conjuncts could never pool — the Filter arm now attempts WITH the filter first (walk_join_children shared prologue).
  2. `resolve_ref_window`'s suffix fallback let QUALIFIED refs bind to same-named columns of OTHER atoms in side-scoped windows (`small.k` → `big1.k`), silently swapping join keys — qualified refs now match dotted-exact + plain-exact only.
  3. Rewritten refs for plain-named (lookup) atoms dropped their qualifier, breaking the INLJ pass's side-aware key extraction — canonical_ref keeps the original qualifier when it names the owning atom (table name or alias).
- Probe (examples/probe_join_order.rs + subquery-atom control): adversarial 3-join (50k×50k×5, point filter on the LAST table) 482 ms → 5.3 ms (~90x), now matching the benign order's 4.5 ms; 100k scale 40k rows out: 63→22 ms; implicit 2-join 50k now hash-joins (was cross+filter); row counts identical to SQLite throughout.
- tests/join_reorder.rs: 13 differential suites vs bundled SQLite — adversarial 3/4-table chains, SELECT * column-order preservation, USING (unqualified `id = id` positional binding), multi-key + mixed-residual ON, LEFT-JOIN boundary pinning (incl. LEFT JOIN (inner spine) and null-extended-side WHERE), subquery atoms declining safely, correlated-subquery WHERE staying on top, self-joins, RowidLookup-driven plans (EXPLAIN asserts SEARCH-first), aggregates/GROUP BY/HAVING/ORDER BY over reordered spines, multiplicity, empties, ANALYZE-estimate-driven reordering, implicit-join fusion (equi + residual-only), CTE atoms.
- Also updated the stale README ledger line for non-UTF-8 files (9636c09 had already closed WHERE range comparisons; index seeks decline-by-design on non-UTF-8 connections).
- Verification: dev matrix 758/758 (lib + all 54 integration suites), fmt clean, clippy -D warnings clean in all 4 configs (default / sqlx / no-default / workspace).

Stage Summary:
- Join reordering + implicit-join WHERE fusion + multi-equi Hash closed: the planner's join orders are now cardinality-driven for INNER spines of 3+ tables, with a ~90x engine-side win on the adversarial order and implicit-join syntax on the hash path.
- 3 latent engine perf bugs fixed on the way: implicit joins as cross+filter, multi-equi ON as nested loops, lookup-side join keys never extracting.
- Remaining planner ledger: predicate pushdown into all scan shapes, subquery decorrelation, bushy/cost-searched plans (greedy can't see everything SQLite's cost model sees).
- Ready to commit + push.

---
Task ID: 9
Agent: main (Super Z)
Task: Deep-research the remaining SQLite gaps, close them, push (user: "clone, deep research, find remaining gaps vs sqlite, fix them all, then push")

Work Log:
- Sandbox reset again: re-cloned iamleson98/rust-sql (PAT auth), reinstalled rust 1.98.1 + rustfmt + clippy. Remote state: master 6b71ae3, CI GREEN on HEAD, local == remote.
- Deep research: wrote examples/probe_master_ddl.rs (38 DDL shapes + 37 pragmas, engine vs bundled-SQLite differential) — found 38 live sqlite_master divergences and 15 PRAGMA divergences in a single run. Also pinned SQLite's exact AUTOINCREMENT/journal_mode/reserved-name contracts with direct probes.
- CLOSED (commit d497a86, tests/schema_parity.rs 24 suites):
  - rootpage convention: schema rows carry SQLite's 1-based file numbers (page 1 = schema b-tree, first user object 2); internal 0-based ids translate at the master-row boundary; every decode->re-encode site converts back (load_schema, ALTER RENAME rewrites, VACUUM compact image, rewrite_schema_row_root).
  - WR-PK autoindex: table-level PK no longer pushes an implicit autoindex on WITHOUT ROWID tables (both execute_create and rebuild_implicit_indexes — composite WR PKs materialized a phantom row SQLite never creates).
  - WR-PK naming: index_list entry is sqlite_autoindex_<t>_<N> with N after every other autoindex (SQLite's observed allocation); rebuild numbers consistently.
  - table_info: WR-PK columns report notnull=1.
  - AUTOINCREMENT: REAL sqlite_sequence(name,seq) — created with the first autoincrement table (rootpage in allocation order), high-water rowid floor (deleted top rowids never reused), explicit-rowid bumps, user re-arm via UPDATE, drop removes the row (table survives), reopen persistence, SQLite's two validation errors pre-mutation, fast/chain INSERT paths gated off.
  - Reserved sqlite_% names: user DDL rejects with SQLite's message; ANALYZE's internal stat1 DDL + foreign replay bypass (guard at the user-facing entry).
  - [bracket] + `backtick` identifier lexing (SQLite's other quoting families), DDL text verbatim.
  - 12 pragma read forms: schema_version (real cookie, +1 per DDL), busy_timeout (+write form), cache_size (raw setting, default -2000), max_page_count, data_version (open-time change counter + 1), journal_mode=memory on :memory: (write AND read), collation_list, database_list (main alone — temp is lazy in SQLite), pragma_list (SQLite's 66 names), compile_options (now 1-based like get()), function_list (136-entry 6-column inventory), module_list (registry).
  - sqlite_compileoption_get: 1-based index fix (was 0-based).
  - DROP TABLE invalidates stale max-rowid/root caches (recreated autoincrement tables start at 1).
  - Fixed 4 pre-existing/stale tests to SQLite's contracts (cache_size read = raw setting; journal_mode write on :memory: = memory).
- CLOSED (commit 78b7b06, compat 61/61): sqlite3_unlock_notify is SQLite-exact — SQLITE_OPEN_SHAREDCACHE arms the table-lock discipline (SQLITE_LOCKED immediate at all three write gates), deferred registration, delivery on the releaser's COMMIT/ROLLBACK thread, per-connection batching (apArg/nArg), close-time cancellation; found + fixed 2 pre-existing compat bugs: await_tx_slot polled forever on busy_timeout=0, and sqlite3_step's DML arm had NO cross-connection tx gate (step-writes silently joined the foreign BEGIN's transaction / deadlocked the writer gate).
- README: fixed the stale planner lines (join reordering + subquery decorrelation were already real; remaining = predicate pushdown + cost-searched plans), rewrote the unlock_notify and sqlite_master/PRAGMA ledger entries as closed, added AUTOINCREMENT + quoting feature bullets.
- Verification: dev matrix 835/835 (lib+integration incl. the 24 new schema_parity suites), compat 61/61, fmt clean, clippy -D warnings clean (default + workspace).

Stage Summary:
- The last queryable sqlite_master/PRAGMA divergences are closed with differential proof; unlock_notify is SQLite-exact.
- 3 more pre-existing bugs found by the new probes: step-path cross-connection tx gate, busy_timeout=0 infinite poll, compileoption_get 0-based indexing.
- Remaining ledger after this pass: cost-searched (bushy) planner, predicate pushdown into all scan shapes, memory peaks (S17/S14), binary size, page_count physical-layout difference (true, not mirrored).
- Ready to push.

---
Task ID: 10
Agent: main (Super Z)
Task: Restore green CI on 44acaf8; close the cost-searched-planner ledger gap (subset DP, bushy plans) with differential proof

Work Log:
- Sandbox reset again: re-cloned iamleson98/rust-sql (PAT), reinstalled rust 1.98.1 + clippy + rustfmt. Remote HEAD 44acaf8 CI RED: all four clippy configs failed on one unused `Value` import in examples/probe_s17_iso.rs (the memory commit shipped without an examples clippy pass). Fixed (16d5e07), pushed — CI back to green.
- Baseline before work: dev matrix 836/836 (--lib --tests, disk-conscious CI env), fmt clean, clippy x4 clean.
- Implemented the cost-searched join order (src/planner/mod.rs, ~600 lines):
  - `try_cost_search_spine`: Selinger-style subset DP replacing the greedy for spines <= DP_LEFTDEEP_MAX_ATOMS (16). Bushy splits (every unordered partition of every subset) for n <= 10 (3^n); left-deep transitions only for 11..=16 (2^n x n); greedy untouched for 17..=64.
  - Cost model: per-step output rows = r1 x r2 x prod(selectivity) where an equi conjunct contributes 1/max(D_a, D_b) (stat1 distinct via the first leading-column index, rowid-alias row count, else SQLite's blind 1/10) and non-equi conjuncts use conjunct_selectivity; hash step = r1+r2+out; nested-loop step = r1*r2+out; pure cartesian = r1*r2. INLJ economics mirror optimize_index_nested_loop_join's own gates exactly (single BARE-Scan inner + eq key with find_index_for_column + outer_is_selective flag propagated through the DP cells — the chain rule): step cost = outer_rows x 3 + out, inner atom's scan base skipped.
  - atom_base_cost (production cost of an access path) distinct from atom_est_rows (output rows); single-atom pooled conjuncts FOLD into their atom as a Filter (pushdown's shape), rows adjust by selectivity.
  - No-op gate: the identity (syntactic) chain is evaluated under the same model — identity is inside the search space, so best <= identity with equal floats iff identity-optimal; rebuild only on >1% win or pooled_from_filter > 0 (WHERE fusion). Same safety gates as the greedy (unique names, no subqueries, outer-join pinning); finish_reordered_spine shared by both paths (restoration Project + top Filter).
  - RSQL_NO_DP=1 env knob (A/B debugging); RSQL_DBG_REORDER prints the DP decision line.
- Bug fixed during implementation: the left-deep DP branch passed the FULL mask as the single-atom side (infinite recursion in reconstruction) — caught by inspection before ever running.
- tests/join_cost_search.rs (13 differential suites vs bundled SQLite): star schema point-filter (EXPLAIN asserts SEARCH fact USING INDEX — the INLJ chain), unfiltered star, ANALYZE'd star GROUP BY, bushy pairing, greedy blind spot, 5-table SELECT * column order + aggregates, non-equi ON, true cartesian, single-atom ON folds, identity no-op, CTE atom, chained INLJ (>= 3 SEARCH lines), plan determinism.
- examples/probe_join_dp.rs (warm best-of-3 both engines, answer-equality asserts):
  - bushy 5k-pairs (ANALYZE): 560.6 ms vs SQLite 1071.5 ms = 1.91x — a plan shape SQLite cannot generate (left-deep only).
  - star 1M INLJ: engine 134 ms vs greedy's 156 ms (1.16x engine-side; SQLite 36 ms — the documented serial-row gap, now with the RIGHT plan under it).
  - blind-spot 100k: 1.09-1.24x vs SQLite.
- All four CI bench gates verified locally (release, gate parser): bench_full_vs_sqlite 18/18 WIN, bench_compare all rows WIN, criterion sqlite_comparison 8/8 WIN (inner_join 1.83x), bench_sqlx_native 12/12 WIN.
- Verification: dev matrix 849/849 (836 + 13 new), fmt clean, clippy -D warnings clean in all 4 configs (default / no-default / sqlx / workspace).

Stage Summary:
- Planner ledger entry CLOSED: cost-searched plans (multi-alternative costing + bushy trees — the engine now searches a plan space STRICTLY LARGER than SQLite's own left-deep-only search).
- Marquee number: 1.91x vs SQLite on the bushy pair-join shape (5M-row output), differentially answer-checked.
- Remaining ledger after this pass: S17/S14 memory peaks (allocator-level), binary size (deliberate), page_count physical layout (true, not mirrored), predicate pushdown into all scan shapes.

---
Task ID: 11
Agent: main (Super Z)
Task: WHERE-pushdown into FROM-clause subqueries (SQLite's pushDownWhereTerms analog) + the EXPLAIN-of-WITH regression the probe surfaced

Work Log:
- CI on the cost-searched planner push (7fa39f0): ALL GREEN (22/22 jobs) — master healthy.
- probe_pushdown (new example): engine-vs-SQLite EXPLAIN over 10 filter-placement shapes — found 2 live gaps: (1) `SELECT * FROM (SELECT ...) s WHERE s.k > 3` scanned the body fully while SQLite pushed the term and searched the index; (2) `EXPLAIN QUERY PLAN WITH c AS (...) SELECT * FROM c` PANICKED with "no such table: c" — the EXPLAIN path used the static plan_for_statement (no CTE scope).
- EXPLAIN-of-WITH fixed: explain_plan_for_statement materializes WITH clauses through the same read-only reader-ctx machinery the execution path uses (both EXPLAIN arms in api.rs rewired). Regression-pinned in tests/pushdown_subquery.rs.
- Pushdown implemented as an AST-level pre-pass in plan_simple_select (the push must precede the body's planning):
  - push_where_into_from_subqueries: clones the FROM, re-homes conjuncts into eligible subquery bodies' WHERE (refs rewritten onto the body's own table through the output->bare-column map), removes them from the outer WHERE.
  - Eligibility (conservative): plain single-table SELECT body (no WITH/compound/DISTINCT/LIMIT/OFFSET/GROUP BY/HAVING/WINDOW/aggregates); every conjunct ref binds to ONE subquery atom — qualified via the atom's FROM alias (which must be unique across the FROM), unqualified only when the name is exposed by exactly one atom of the whole FROM; only ref-through shapes (Column/Collate/Binary/Unary/Between/In-list/Like/Literal/Parameter); no subquery-carrying conjuncts; atoms under any outer-join boundary decline.
  - Star/TableStar projections expand through the body table's columns; FROM column aliases rename positionally; non-bare outputs (expressions) decline.
  - the pre-pass operates on the COLLATED where; the body re-collates its own WHERE against its own scope when planned.
- Probe results after: `SEARCH t USING INDEX idx_t_k (k>?)` — EXACTLY SQLite's plan for both the projection and SELECT * bodies.
- tests/pushdown_subquery.rs (16 differential suites vs bundled SQLite): pushes (qualified/unqualified/star/multi-conjunct/IN-list/parameter/join-alongside), declines (expression outputs, aggregate bodies, LIMIT bodies — filter-then-limit is NOT limit-then-filter, DISTINCT, compound bodies, LEFT-join boundaries both sides, ambiguous unqualified — pinned via EXPLAIN no-index + a disjoint-value shape), nested subquery bodies, and the EXPLAIN-of-WITH regression.
- Found + documented while testing: the engine's parser does not accept FROM-subquery column aliases `s(a, b)` (SQLite does) — noted as a parser gap; the engine resolves ambiguous unqualified refs first-match instead of erroring like SQLite (pre-existing, plain tables identical — out of scope).
- Verification: dev matrix 865/865 (849 + 16), fmt clean, clippy -D warnings clean in all 4 configs, all four CI bench gates PASS locally (18/18, 20/20, 8/8, 12/12 WIN).

Stage Summary:
- The classic subquery pushdown optimization is REAL and SQLite-plan-matching for the eligible shapes; EXPLAIN-of-WITH no longer errors.
- Remaining pushdown surface (documented): compound bodies, CTE atoms (eagerly materialized before planning), view bodies (expanded during planning), join-condition pushdown/ flattening (SQLite's covering-index trick over subquery join sides).
- Ready to push.

---
Task ID: 12
Agent: main (Super Z)
Task: Compound-body pushdown (UNION/INTERSECT/EXCEPT arms) + the FROM-subquery column-list fix the tests surfaced

Work Log:
- Found while restoring the column-aliases test: the engine PARSES PostgreSQL-style FROM-subquery column lists s(a, b) (a superset — SQLite itself rejects the syntax) but planning ignored the list — outer refs to the declared names projected NULL. Fixed: positional Project over the subquery boundary, the same shape CREATE VIEW v(a, b) uses (83271c8, pushed, CI running).
- Compound-body pushdown: a row filter DISTRIBUTES over set operations (filter(A UNION B) = filter(A) UNION filter(B), likewise INTERSECT/EXCEPT), so the same term is rewritten into EVERY arm — each arm's own table refs through its own output mapping, all-or-nothing (a partially-filtered compound changes the set-op result).
  - subquery_push_map now returns SubqueryPushTarget: Simple(map) | Compound(per-arm maps) — compound arms normalize their output NAMES to the leftmost arm's (shared positions), keep their own targets; arity must match (SQLite's compound rule); FROM column lists rename positionally over the compound.
  - push_into_subquery_atoms: validation against the shared outputs (a position is pushable only when EVERY arm maps it to a bare column), then per-arm rewrite + AND into each Simple leaf in left-to-right order (and_into_leaves).
  - rewrite_conjunct_into_body split: conjunct_binds_to_atom (validation, shared by both paths) + rewrite_refs_to_body (per-arm substitution).
  - Inventory: compound subquery atoms expose the leftmost arm's names (stars expanded through the arm's single table) — subquery_atom_output_names / simple_select_names.
- Probe after: engine shows SEARCH t USING INDEX idx_t_k (k>?) + SCAN u + temp b-tree — SQLite's own compound pushdown shape (SQLite additionally materializes a co-routine wrapper; the engine's plan is flatter).
- tests/pushdown_subquery.rs: compound suites (UNION ALL with per-arm different tables+indexes, EXPLAIN asserts both arms' index searches; UNION; INTERSECT; EXCEPT; decline with an ineligible aggregated arm — pinned by EXPLAIN no-index) — 18 suites total.
- Verification: dev matrix 867/867, fmt clean, clippy -D warnings clean in all 4 configs.

Stage Summary:
- The pushdown surface now covers plain and compound subquery bodies; remaining: CTE atoms (architectural — eager materialization), view bodies (expanded mid-planning), join-condition pushdown/flattening.
- Ready to push.

---
Task ID: 13
Agent: main (Super Z)
Task: S17/S14 memory-gap forensics (allocator level) — close the question with measurements

Work Log:
- Measured current state (CI lean env: MIMALLOC_ARENA_RESERVE=64M, ALLOW_THP=0): S17 hwm 12.56 MB vs SQLite 6.99 (engine 2.44x FASTER on open+SUM: 24.2ms vs 59.0ms); S14 hwm 12.97 vs 8.51.
- Phase attribution (instrumented drop/reopen probe): build phase ~8.85 MB (base ~2.9 + the 2 MB live page cache — parity with SQLite's default — + ~3 MB insert-path retention + commit step); the 3x open+SUM cycles stack +2.5 MB on top because the build's freed pages never return to the OS; parallel SUM itself costs +1.7 MB over serial (worker allocations across mimalloc size classes).
- Drop-drain experiment: added drain_mimalloc_wake() to Database::drop — S17 moved only 12.56 -> 12.25 MB. Direct micro-probes of mi_collect(true) under the engine's purge_delay = -1 configuration: ZERO pages returned for freshly-freed blocks (across drop / settle-300ms / repeated wake+collect cycles; one abandoned-segment state returned 2.3 MB, not reproducible). The drain was a measured no-op — REVERTED (no cost-without-benefit code).
- Conclusion recorded in the README ledger: the S17/S14 deltas are mimalloc holding RSS at the heap's high-water (the deliberate 20-40% small-allocation throughput trade; default-features=false recovers glibc). purge_delay >= 0 would auto-return pages but re-faults the working set every benchmark round (measured +0.5ms/round on the join bench) — the existing trade stands.
- The 44acaf8 commit already took the achievable wins (checkpoint scratch reuse: commit-step +2.6 -> +0.7 MB).

Stage Summary:
- The "each remaining MB needs profiling at the allocator level" ledger line is now closed WITH the profiling: the remaining S17/S14 gap is the allocator floor, documented with direct measurements. No code change shipped (the honest outcome); README + worklog updated.
- Ready to push (docs-only commit).

---
Task ID: 14
Agent: main (Super Z)
Task: CTE pushdown — WHERE conjuncts into single-use CTE bodies before materialization

Work Log:
- Planner: push_where_into_cte_bodies (pub(crate)) — clone + mutate the statement: candidates are CTEs referenced EXACTLY ONCE anywhere in the statement (count_cte_refs_* walker: main FROM, nested FROMs, CTE bodies, expression subqueries — scalar/EXISTS/IN); the single reference must be a main-FROM Table atom (no INDEXED hint) not under an outer-join boundary; the body passes the same eligibility as the subquery pre-pass (plain single-table SELECT, no WITH/LIMIT/OFFSET/DISTINCT/grouping/windows/aggregates); the conjunct binds through the CTE's exposed names (declared column list or the body's own) with the same qualifier-uniqueness and unqualified-uniqueness rules; rewrite_refs_to_body rewrites onto the body's table; and_into_select_body ANDs it into the body's WHERE. The main WHERE loses the pushed conjuncts.
- api.rs exec_select_with_ctes: runs the pre-pass on the incoming statement (non-recursive WITH only) and materializes/plan the modified clone — the materialized set is pre-filtered and the body's own planning sees the term (index ranges, rowid lookups).
- CTE-aware inventory (collect_cte_aware_inventory/cte_output_names): a Table atom naming a CTE exposes the CTE's output names (stars expanded through the body's table).
- The EXPLAIN path materializes + plans without the pre-pass — the displayed plan node is CteRows either way ("SCAN CTE" — the honest shape: the rows are pre-filtered; SQLite's co-routine display shape differs by architecture).
- tests/pushdown_subquery.rs: 10 CTE suites (28 total): pushes (qualified/unqualified/declared-column-list/under INNER join/prefiltered-materialization SUM), declines (referenced twice via a WHERE subquery — the shared-materialization corruption case, used twice in FROM, RECURSIVE, aggregate body, LIMIT body, LEFT-join boundary).
- Timing (1M-row CTE SUM, k > 500000): engine 101ms vs SQLite 55ms — the CteRows materialization model dominates (the push removes the post-filter and halves the materialized set, but the engine materializes rows where SQLite's co-routine streams); answers equal.
- Verification: dev matrix 877/877, fmt clean, clippy -D warnings clean in all 4 configs.

Stage Summary:
- The pushdown surface now covers plain subqueries, compound bodies, and single-use CTEs; remaining: multiply-referenced CTEs (architectural), view bodies (expanded mid-planning), join-condition pushdown/flattening.
- Ready to push.
