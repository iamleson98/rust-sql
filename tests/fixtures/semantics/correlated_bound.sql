-- A WHERE comparison whose value side holds a subquery CORRELATED to the
-- scanned row is not an index bound (it is evaluated per row). Subqueries
-- under CAST / COLLATE / BETWEEN / IS / LIKE / IN lists were invisible to
-- the correlation check: `k < CAST((SELECT ... WHERE g < l) AS TEXT)` was
-- planned as `SEARCH t3 USING INDEX t3_k (k<?)` and returned no rows.
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER);
CREATE TABLE t3(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
CREATE INDEX t2_g ON t2(g);
CREATE INDEX t3_k ON t3(k);
CREATE TABLE r(a INTEGER PRIMARY KEY, k INTEGER, l);
CREATE INDEX r_k ON r(k);
INSERT INTO t2(g) VALUES (5), (-3), (4), (9), (NULL);
INSERT INTO t3 VALUES ('a', -5, 1e308), ('b', 3, -1), ('c', -3, NULL), ('d', 1, 6), ('e', 8, 10);
INSERT INTO r SELECT rowid, k, l FROM t3;
SELECT j FROM t3 WHERE k < CAST((SELECT max(g) FROM t2 WHERE g < l) AS VARCHAR(5)) ORDER BY j;
SELECT j FROM t3 WHERE k < CAST((SELECT max(g) FROM t2 WHERE g < l) AS INTEGER) ORDER BY j;
SELECT j FROM t3 WHERE CAST((SELECT max(g) FROM t2 WHERE g < l) AS TEXT) > k ORDER BY j;
SELECT j FROM t3 WHERE k >= (SELECT min(g) FROM t2 WHERE g > l) COLLATE NOCASE ORDER BY j;
SELECT j FROM t3 WHERE k = CAST((SELECT max(g) FROM t2 WHERE g < l) AS INTEGER) ORDER BY j;
SELECT j FROM t3 WHERE k BETWEEN -10 AND CAST((SELECT max(g) FROM t2 WHERE g < l) AS INTEGER) ORDER BY j;
SELECT j FROM t3 WHERE k IN (CAST((SELECT max(g) FROM t2 WHERE g < l) AS INTEGER), 100) ORDER BY j;
SELECT a FROM r WHERE k < CAST((SELECT max(g) FROM t2 WHERE g < r.l) AS TEXT) ORDER BY a;
SELECT a FROM r WHERE a < CAST((SELECT count(*) FROM t2 WHERE g < l) AS INTEGER) + 3 ORDER BY a;
SELECT a FROM r WHERE k < CAST((SELECT max(g) FROM t2 WHERE g < 5) AS TEXT) ORDER BY a;
