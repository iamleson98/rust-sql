# rustqlite

An embedded SQL database written from scratch in Rust, built to match SQLite's behaviour and beat it. Every comparison below is against the bundled SQLite 3.53.4, with both engines doing identical work and their answers checked equal before timing.

## Beats SQLite

- **Speed**: wins every time column of the 18-section torture matrix and all 18 rows of `bench_full_vs_sqlite`. Examples: full-scan aggregates 10x, top-N `ORDER BY` 9x, `GROUP BY` 4.6x, `LIKE '%…%'` scans 3x, large `IN` lists 3x, point lookups 1.2x, bulk load 1.4x, joins on computed keys (`b.y = a.x + 1`) 1.1–1.7x, cold open of a 1M-row file 2x.
- **Concurrency**: parallel readers 4–18x, parallel writers (`BEGIN CONCURRENT`) 3.2x, 8 readers + 2 writers 2.0x, 8-connection mixed read/write via sqlx 7–8x, single-query parallel aggregates 5.8–8.8x.
- **Scale**: a 100M-row run against SQLite every night on Linux, Windows and macOS; the 100M-row file ends up 5% smaller than SQLite's.
- **Correctness**: about 1,800 tests, including crash recovery, out-of-memory and I/O fault injection, corruption fuzzing, and differential fuzzing against real SQLite (over a million queries, compared value-for-value by storage class).

## Still loses to SQLite

- **Memory**: peak memory is 1–4 MB higher on most torture sections, and 15 MB higher on `GROUP BY` over 100k groups (57 vs 42 MB). The binary is about 1.5x larger (mimalloc and extra features).
- **Speed**:
  - mass `UPDATE` of an indexed column on macOS: about 1.4x slower at 2M rows, up to 1.95x at 100M rows on the CI Mac machines;
  - 4 readers + 1 writer on the CI Mac machines: 0.58–0.67x (it wins on x86);
  - `GROUP BY` over 100k groups at the nightly scale-matrix size on Windows: 213 vs 140 ms.

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

License: MIT OR Apache-2.0.
