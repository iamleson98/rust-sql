-- SQLite's OUTER JOIN strength reduction: a LEFT JOIN whose right table
-- cannot be the NULL row under the WHERE (sqlite3ExprImpliesNonNullRow)
-- is an ordinary JOIN, so its ON terms become WHERE terms — a constant
-- one is evaluated once before the loop and raises even when no pair
-- reaches it. Terms that do not imply a non-NULL row (IS NULL, functions,
-- CASE, OR with a non-implying arm, IN (subquery)) keep the LEFT JOIN,
-- and its ON terms evaluate only for the pairs it reaches.
CREATE TABLE t1(a INTEGER, b TEXT);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, i REAL);
CREATE TABLE t3(k INTEGER, m TEXT);
INSERT INTO t1 VALUES (1, 'x'), (2, 'y'), (NULL, 'z');
INSERT INTO t2 VALUES (10, 1, 2.0), (11, 2, NULL);
INSERT INTO t3 VALUES (1, 'p'), (5, 'q');
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND (abs(-9223372036854775808) >> 0) WHERE (i >> id);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g + 1 > 0 AND t1.b <> 'q';
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t1.b <> 'q' AND t2.g NOTNULL;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g NOTNULL AND t1.b <> 'q';
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE likely(t2.g > 0);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t1.b <> 'q' AND likely(t2.g > 0);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g IS NULL;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE coalesce(t2.g, 0) > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g > 0 OR t1.a > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g > 0 OR t2.i < 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE CASE WHEN t2.g THEN 1 END;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g IN (1, 2);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g IN (SELECT k FROM t3);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g BETWEEN 0 AND 5;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE 3 BETWEEN t2.g AND t1.a;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE NOT (t2.g IS NULL);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g LIKE '1%';
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE -t2.g < 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE CAST(t2.g AS TEXT) = '1';
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE g > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.rowid > 0;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) JOIN t3 ON t3.k = t2.g;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) LEFT JOIN t3 ON t3.k = t2.g WHERE t3.m > '';
SELECT count(*) FROM t1 LEFT JOIN t3 ON t3.k = t1.a LEFT JOIN t2 ON t2.id = t3.k AND abs(-9223372036854775808) WHERE t2.g > 0;
SELECT count(*) FROM t1 LEFT JOIN (SELECT id AS sid, g AS sg FROM t2) s ON t1.a = s.sid AND abs(-9223372036854775808) WHERE s.sg > 0;
SELECT t1.a, t2.g FROM t1 LEFT JOIN t2 ON t1.a = t2.g WHERE t2.g > 0 ORDER BY 1;
SELECT t1.a, t2.g FROM t1 LEFT JOIN t2 ON t1.a = t2.g WHERE t2.g IS NULL ORDER BY 1;
SELECT t1.a, t2.g, t3.m FROM t1 LEFT JOIN t2 ON t1.a = t2.g LEFT JOIN t3 ON t3.k = t2.g WHERE t3.m > '' ORDER BY 1;
SELECT t1.a, t2.g, t3.m FROM t1 LEFT JOIN t2 ON t1.a = t2.g LEFT JOIN t3 ON t3.k = t1.a WHERE t2.g + t3.k > 0 ORDER BY 1;
