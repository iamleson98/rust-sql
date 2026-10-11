-- whereLoopAddBtree costs a table's indexes in Table.pIndex order:
-- newest first (sqlite3CreateIndex prepends each one), REPLACE indexes
-- last. The order decides whereLoopAdjustCost: a full index scan
-- inserted AFTER an equality probe it is a proper subset of is raised
-- to that probe's cost. Here ix21 (created after ANALYZE, default
-- estimates; it also floors t0's row estimate at 1000) is costed before
-- the analyzed 1-row ix14, so the ix14 full scan costs 51, not 13, and
-- SQLite scans b and probes a.e through ix14 — a comparison, under which
-- the boundary REAL -9.2233720368547758e18 equals i64::MIN — instead of
-- seeking b's rowid (OP_SeekRowid refuses that REAL; ticket #3922).
-- Stateful-fuzz seed 592592 case 49.
CREATE TABLE t0 (c REAL, e REAL NOT NULL);
CREATE TABLE t1 (d INTEGER PRIMARY KEY, a INTEGER);
INSERT INTO t1 (d, a) VALUES (-9223372036854775808, 20), (42, -9223372036854775808), (-9, NULL);
CREATE INDEX ix14 ON t0(e);
INSERT INTO t0 (e) VALUES (-9.2233720368547758e18);
ANALYZE;
CREATE INDEX ix21 ON t0(e);
/*ordered*/ SELECT a.rowid, b.rowid FROM t0 a JOIN t1 b ON a.e = b.d ORDER BY 1, 2 LIMIT 200;
/*ordered*/ SELECT a.rowid, b.rowid FROM t1 b JOIN t0 a ON a.e = b.d ORDER BY 1, 2;
SELECT count(*) FROM t0 a JOIN t1 b ON b.d = a.e;
SELECT b.a FROM t0 a, t1 b WHERE a.e = b.d;
-- the same tables with the later index dropped: SQLite seeks b's rowid
DROP INDEX ix21;
/*ordered*/ SELECT a.rowid, b.rowid FROM t0 a JOIN t1 b ON a.e = b.d ORDER BY 1, 2 LIMIT 200;
SELECT count(*) FROM t0 a JOIN t1 b ON b.d = a.e;
-- and re-created before the analyzed one is dropped and re-created
CREATE INDEX ix21 ON t0(e);
DROP INDEX ix14;
CREATE INDEX ix14 ON t0(e);
/*ordered*/ SELECT a.rowid, b.rowid FROM t0 a JOIN t1 b ON a.e = b.d ORDER BY 1, 2 LIMIT 200;
SELECT count(*) FROM t0 a JOIN t1 b ON b.d = a.e;
ANALYZE;
/*ordered*/ SELECT a.rowid, b.rowid FROM t0 a JOIN t1 b ON a.e = b.d ORDER BY 1, 2 LIMIT 200;
SELECT count(*) FROM t0 a JOIN t1 b ON b.d = a.e;
