-- coalesce / ifnull / iif are coded inline and LAZILY (untaken arguments never
-- evaluate); the COLLATE search follows SQLite's expression shapes (BETWEEN /
-- IN lists, LIKE as like(pattern, expr)); LIKE is a function for the parse-
-- time AND fold; X IS [NOT] [DISTINCT FROM] NULL folds over literals.
CREATE TABLE t(a INTEGER, b TEXT COLLATE NOCASE, c TEXT, d INTEGER);
INSERT INTO t VALUES (1, 'x', 'B', -9223372036854775808), (2, 'Y', 'a', 5), (3, 'z', 'C', 6), (4, NULL, 'b', NULL);
SELECT a, iif(CAST(0 AS BLOB), abs(d), 'no') FROM t;
SELECT a, iif(d > 0, d, abs(-9223372036854775808)) FROM t WHERE d > 0;
SELECT a, coalesce(a, abs(-9223372036854775808)) FROM t;
SELECT a, ifnull(a, abs(d)) FROM t;
SELECT a, iif(0, abs(d), 1, 2) FROM t;
SELECT iif(1, 'y'), iif(0, 'y'), iif(NULL, 1, 0), iif(x'30', 1, 0), iif(x'31', 1, 0), iif('0.0', 1, 0);
/*ordered*/ SELECT c || (a BETWEEN 0 AND (c COLLATE NOCASE)) AS k FROM t ORDER BY 1;
/*ordered*/ SELECT c || (a IN (1, c COLLATE NOCASE)) AS k FROM t ORDER BY 1;
/*ordered*/ SELECT c || (c LIKE (b COLLATE NOCASE)) AS k FROM t ORDER BY 1;
/*ordered*/ SELECT (c COLLATE NOCASE) LIKE b, c FROM t ORDER BY 2;
SELECT ((0 AND a) AND (c LIKE 'a')), count(*) FROM t GROUP BY ((0 AND a) AND (c LIKE 'a'));
SELECT (0 AND a), count(*) FROM t GROUP BY (0 AND a) HAVING count(*) > 0;
SELECT count(*) FROM t WHERE 0 AND c LIKE 'x';
SELECT (-1e308 IS NOT DISTINCT FROM NULL), count(*) FROM t GROUP BY (-1e308 IS NOT DISTINCT FROM NULL);
SELECT (-5 IS NULL), count(*) FROM t GROUP BY (-5 IS NULL);
SELECT (5 IS NOT NULL) AS q FROM t ORDER BY (5 IS NOT NULL);
SELECT (-1e308 IS NOT DISTINCT FROM NULL) AS q, count(*) FROM t GROUP BY q;
SELECT a, (a IS (NULL)), (a IS NOT (NULL)), (a IS DISTINCT FROM NULL), (5 IS NOT DISTINCT FROM (NULL)) FROM t;
SELECT count(*) FROM t GROUP BY ('x' IS DISTINCT FROM NULL);
