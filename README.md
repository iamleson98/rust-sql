# rustqlite

An embedded SQL database written from scratch in Rust, built to match SQLite's behaviour and beat it. Every comparison below is against the bundled SQLite 3.53.4, with both engines doing identical work and their answers checked equal before timing.

## Beats SQLite

- **Speed**: wins all 18 rows of `bench_full_vs_sqlite`, every row of the other three CI benchmark gates (`bench_compare`, the criterion comparison, `bench_sqlx_native`), and 13 of the 15 timed sections of the 18-section torture matrix (random-key inserts are at parity; the five-index load is listed below). Examples: full-scan aggregates 10x, top-N `ORDER BY` 9x, `GROUP BY` 4.6x, `LIKE '%…%'` scans 3x, large `IN` lists 3x, point lookups 1.2x, bulk load 1.4x, joins on computed keys (`b.y = a.x + 1`) 1.1–1.7x, cold open of a 1M-row file 2x.
- **Concurrency**: parallel readers 4–18x, parallel writers (`BEGIN CONCURRENT`) 3.2x, 8 readers + 2 writers 2.0x, 8-connection mixed read/write via sqlx 7–8x, single-query parallel aggregates 5.8–8.8x.
- **Scale**: a 100M-row run against SQLite every night on Linux, Windows and macOS; the 100M-row file ends up 5% smaller than SQLite's.
- **Correctness**: about 1,800 tests, including crash recovery, out-of-memory and I/O fault injection, corruption fuzzing, and differential fuzzing against real SQLite (over a million queries, compared value-for-value by storage class).

## Still loses to SQLite

- **Memory**: peak memory is 1–4 MB higher on most torture sections, and 15 MB higher on `GROUP BY` over 100k groups (57 vs 42 MB). The default page cache is 8 MB against SQLite's 2 MB (`PRAGMA cache_size` lowers it): file-backed workloads larger than 2 MB can use up to 6 MB more. The binary is about 1.5x larger (mimalloc and extra features).
- **Speed**:
  - mass `UPDATE` of an indexed column on macOS: about 1.4x slower at 2M rows, up to 1.95x at 100M rows on the CI Mac machines;
  - 4 readers + 1 writer on the CI Mac machines: 0.58–0.67x (it wins on x86);
  - `GROUP BY` over 100k groups at the nightly scale-matrix size on Windows: 213 vs 140 ms;
  - loading a table with five secondary indexes one row per statement (torture S12): about 0.8x on macOS (300 vs 240 ms for 100k rows). Integer index keys take 11 bytes against SQLite's 1–4, so the indexes split and rebalance more often.

## Remaining gaps

- **SQLite's own test suites** (run weekly and on demand by the `SQLite suites` workflow): sqllogictest passes in full (5.7M queries); in SQLite's Tcl suite, about 14% of the tests that pass on SQLite still fail here, mostly in 3 files that time out, plus a ~1% tail across about 370 files. The fault-injection Tcl files aren't run yet.

- **Unspecified results can differ**: which of several equal values (`1` / `1.0`, `'a'` / `'A'` under NOCASE) represents a `DISTINCT` / `GROUP BY` group or a `min()`/`max()`, and the order of rows that tie in `ORDER BY`. SQLite doesn't define these either.
- **Error timing can depend on the plan**: an expression that raises an error inside a join condition, or under `ORDER BY … LIMIT`, can fail here where SQLite's chosen plan never evaluates it. Rows returned never differ.
- **Storage**: the on-disk format is rustqlite's own. SQLite files are read and written as an interchange format, not used live. WAL-mode files support multiple processes; rollback-journal files support only one.
- **Missing or different features**:
  - temp and main tables share one namespace;
  - compiled SQLite extensions (`.so`) don't load (rustqlite has its own extension ABI);
  - `BEGIN CONCURRENT` rejects DDL and PRAGMA;
  - commits across attached databases aren't atomic (they run one after another);
  - statements spanning attached databases don't use the attached databases' indexes;
  - `sqlite_dbdata` exposes a different page layout.

## Known bugs and open work

Numbers come from the last full Tcl-suite run, which predates the latest join and WAL fixes. Re-run the `SQLite suites` workflow for current numbers (Actions → SQLite suites → Run workflow; `tcl_files` takes globs such as `window*.test`). To reproduce one file locally against a checkout of SQLite's repository:

```bash
python3 tests/tcl/run.py <sqlite>/test window3.test -j 1 -o out.json
```

`out.json` holds each failing test's expected and actual output. Before committing a fix, run `cargo test --lib --tests`, clippy (`--all-targets -- -D warnings`) and `cargo test --release --test sqlite_semantics_battery`. Pin regressions as tests or as fixtures under `tests/fixtures/semantics/`.

- **CI**: the first Tcl-suite smoke job on Linux runners hadn't finished when last checked; confirm the `SQLite suites` workflow runs green end to end.
- **Speed and memory losses**: see "Still loses to SQLite" above.

- **Tcl suite timeouts**:
  - `withM`: its fault loops run at about 83% of SQLite's speed and hit the time limit;
  - `tkt-9f2eb3abac`: times out on SQLite too;
  - `tkt-313723c356`: was a real stale-read bug, fixed in 53a0353; needs a re-run to confirm.
- **Tcl suite divergences** (number of failing tests in brackets):
  - window functions: `window3` (417), `window8` (144);
  - foreign keys: `fkey2` (243), `without_rowid3` (240), `e_fkey` (175);
  - SELECT: `selectB` (169), `e_select` (93), `selectA` (82), `select1` (50, e.g. `min()` / `max()` with the wrong number of arguments isn't an error);
  - DDL: `e_createtable` (159), `alter*`;
  - other: `keyword1` (116, keyword parsing), `json501` (114, JSON5), `e_insert` (100), `sidedelete` (98), `ctime` (66), `pragma` (56), `upsert5`, `strict1`, `literal`, `nulls1`, `rowvalue`, `autoinc`, plus a tail of under 50 per file across about 350 more files.
- **Tcl files not run**: the fault-injection files (`malloc*`, `*fault*`, `crash*`, `ioerr*`, `oom*`). They need testfixture C hooks that the test shim doesn't provide.
- **Behaviour differences**:
  - positional parameters (`?`) don't fill named ones (`:a`, `$a`) by index;
  - an unused CTE that contains an error is still evaluated, so it raises an error SQLite doesn't;
  - `UNION ALL` push-down can apply column affinity differently;
  - the same unaliased table joined to itself with `NATURAL` behaves differently;
  - after `ROLLBACK TO` inside `BEGIN CONCURRENT`, rowids aren't reused (by design: the allocator is shared).
- **Concurrency**:
  - two connections in one process can't take turns writing: the first writer keeps the writer role until it closes, while SQLite lets them alternate;
  - a two-connection open / read / update / close cycle takes 6 ms, against 1.3 ms for SQLite.

License: MIT OR Apache-2.0.
