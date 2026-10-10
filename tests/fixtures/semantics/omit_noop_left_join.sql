-- SQLite's whereOmitNoopJoin: a LEFT JOIN whose right table is used
-- nowhere but its own ON, and that matches at most one row per left row
-- (an equality on its INTEGER PRIMARY KEY / rowid) or sits in a DISTINCT
-- query, is not run — its ON terms are never evaluated. Any other use of
-- the table (result, ORDER BY, WHERE, another ON), a non-unique key, or
-- an aggregate query keeps the join and evaluates the ON for the pairs
-- it reaches. (Whether a non-unique key's ON terms run on EVERY inner row
-- depends on SQLite building an automatic index there — plan-dependent,
-- not pinned here.)
CREATE TABLE t1(a INTEGER, f TEXT);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT);
CREATE TABLE t3(k INTEGER PRIMARY KEY, m TEXT);
INSERT INTO t1 VALUES (2, 'x'), (7, 'y'), (2, 'z'), (NULL, 'w');
INSERT INTO t2 VALUES (2, 20, 'p'), (3, 30, 'q');
INSERT INTO t3 VALUES (20, 'm20'), (7, 'm7');
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND trim(abs(-9223372036854775808)) ORDER BY f;
SELECT 1 FROM t1 LEFT JOIN t2 ON t2.id = t1.a AND abs(-9223372036854775808);
SELECT f FROM t1 LEFT JOIN t2 ON t2.rowid = t1.a AND abs(-9223372036854775808) ORDER BY 1;
SELECT f FROM t1 LEFT JOIN t2 ON t2.id = 2 AND abs(-9223372036854775808) ORDER BY 1;
SELECT DISTINCT f FROM t1 LEFT JOIN t2 ON t2.g = t1.a AND t2.id = 3 AND abs(-9223372036854775808);
SELECT DISTINCT f FROM t1 LEFT JOIN t2 ON t2.g = t1.a AND t2.id = 3 AND abs(-9223372036854775808) ORDER BY 1 DESC;
SELECT DISTINCT f FROM t1 LEFT JOIN t2 ON t2.g = t1.a AND t2.id = 3 AND abs(-9223372036854775808) ORDER BY f || '';
SELECT t1.f FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) LEFT JOIN t3 ON t3.k = t1.a AND abs(-9223372036854775808) ORDER BY 1;
SELECT f, t2.g FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808);
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) ORDER BY t2.g;
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) WHERE t2.g IS NULL;
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id + 0 AND abs(-9223372036854775808);
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808);
SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808);
SELECT t1.* FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808) ORDER BY f;
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id LEFT JOIN t3 ON t3.k = t2.g AND abs(-9223372036854775808) ORDER BY f;
SELECT f, t3.m FROM t1 LEFT JOIN t2 ON t1.a = t2.id LEFT JOIN t3 ON t3.k = t2.g ORDER BY f;
SELECT f FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND t2.h = 'p' ORDER BY f;
SELECT f, (SELECT count(*) FROM t3) FROM t1 LEFT JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808);
SELECT f FROM t1 LEFT JOIN t2 ON id = a AND abs(-9223372036854775808) ORDER BY f;
SELECT f FROM t1 LEFT JOIN t2 AS x ON x.id = t1.a AND abs(-9223372036854775808) ORDER BY f;
