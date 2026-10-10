-- An INNER join's ON terms are WHERE terms (sqlite3ProcessJoin appends
-- them, in FROM order, after the WHERE's own). Each loop level tests its
-- terms in that order, so a WHERE term runs BEFORE an ON term at the same
-- level: it can raise on a pair (or row) the ON term would reject. An
-- equality the level SEEKS with (an index, or SQLite's automatic index)
-- rejects pairs before any other term runs.
CREATE TABLE t1(a INTEGER, b TEXT, c REAL);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT);
INSERT INTO t1 VALUES (1, 'x', 1.5), (2, 'y', 2.5), (3, 'z', NULL);
INSERT INTO t2 VALUES (10, 1, 'p'), (11, 2, 'q');
CREATE TABLE t3(k INTEGER, m TEXT);
INSERT INTO t3 VALUES (5, 'x'), (6, 'z');
SELECT count(*) FROM t1 JOIN t2 ON t1.a < 0 WHERE abs(t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 JOIN t2 ON t2.g < 0 WHERE abs(t2.g*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 JOIN t2 ON t1.a = t2.g + 100 WHERE abs(t2.g*0 + t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 JOIN t2 ON t1.a + t2.g < 0 WHERE abs(t2.g*0 + t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1, t2 WHERE t1.a + t2.g < 0 AND abs(t2.g*0 + t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1, t2 WHERE abs(t2.g*0 + t1.a*0 - 9223372036854775807 - 1) > 0 AND t1.a + t2.g < 0;
SELECT count(*) FROM t1 JOIN t2 ON abs(t2.g*0 + t1.a*0 - 9223372036854775807 - 1) > 0 WHERE t1.a + t2.g < 0;
SELECT count(*) FROM t1 JOIN t2 ON t1.a < 0 AND t2.g < 0 WHERE abs(t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a + t2.g < 0 WHERE abs(t1.a*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 JOIN t2 ON t1.a IN ('n', t1.b, t2.id) WHERE CASE WHEN abs(-9223372036854775808) THEN t2.h < t1.c END;
SELECT count(*) FROM t1 JOIN t2 ON t1.a IN ('n', t1.b, t2.id) WHERE CASE WHEN abs(-5) THEN t2.h < t1.c END;
SELECT count(*) FROM t1 JOIN t2 ON t2.g = t1.a JOIN t3 ON t3.k = t2.id + t1.a WHERE abs(t3.k*0 + t2.g*0 - 9223372036854775807 - 1) > 0;
SELECT count(*) FROM t1 JOIN t2 ON t2.g = t1.a JOIN t3 ON t3.k + t2.id > 100 WHERE abs(t3.k*0 + t2.g*0 - 9223372036854775807 - 1) > 0;
SELECT t1.a, t2.id, t3.k FROM t1 JOIN t2 ON t2.g = t1.a JOIN t3 ON t3.m = t1.b ORDER BY 1, 2, 3;
SELECT t1.a, t2.id FROM t1 JOIN t2 ON t2.g = t1.a AND t2.h = 'p' WHERE t1.b <> 'z' ORDER BY 1, 2;
