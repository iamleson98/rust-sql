-- A column projected more than once: the fused paths MOVED each decoded
-- value into the output, so the second reference read NULL (index nested-
-- loop join with selective inner decode; parallel sort merge).
CREATE TABLE t1(a INTEGER, f);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, i REAL);
CREATE INDEX t2_g ON t2(g);
INSERT INTO t1 VALUES (1, 1), (2, 0), (3, 1);
INSERT INTO t2(g, i) VALUES (1, 1.5), (2, 2.5), (3, 3.5);
SELECT i, i FROM t1 JOIN t2 ON t1.a = t2.g WHERE f;
SELECT i, i FROM t1 JOIN t2 ON t1.a = t2.g;
SELECT i, g, i FROM t1 JOIN t2 ON t1.a = t2.g WHERE f;
SELECT a, a FROM t1 JOIN t2 ON t1.a = t2.g WHERE f;
SELECT t2.i, i, t1.a, a FROM t1 JOIN t2 ON t1.a = t2.g WHERE f;
SELECT i, i FROM t1, t2 WHERE t1.a = t2.g AND f;
SELECT i, i FROM t2 WHERE g > 1;
CREATE TABLE big(x INTEGER, y TEXT);
WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM n WHERE v < 30000) INSERT INTO big SELECT v % 977, 'r' || v FROM n;
/*ordered*/ SELECT y, y, x FROM big ORDER BY x, y LIMIT 20;
SELECT count(*), count(DISTINCT y1 || y2) FROM (SELECT y AS y1, y AS y2, x FROM big ORDER BY x DESC, y);
/*ordered*/ SELECT x, y, y, x FROM big WHERE x > 970 ORDER BY y;
