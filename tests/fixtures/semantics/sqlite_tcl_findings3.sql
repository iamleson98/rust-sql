-- Found by SQLite's TCL suite (select1, select4, select6, selectA, view):
-- * a result alias resolves inside WHERE / ON / GROUP BY / HAVING /
--   ORDER BY expressions when no FROM column has the name;
-- * compound ORDER BY terms match ANY arm's result columns (leftmost
--   first), star arms included;
-- * `SELECT * FROM (SELECT * FROM t WHERE k = ?)` over an index lookup
--   leaked the hidden rowid as an extra column;
-- * string literals as column / table aliases; `t.c "x"` aliases;
-- * prepare-time errors: unknown functions, f(*) arity, aggregates in
--   WHERE / nested in aggregates / in a non-aggregate ORDER BY, aliased
--   aggregates where none may appear.
CREATE TABLE test1(f1 int, f2 int);
INSERT INTO test1 VALUES(11,22);
INSERT INTO test1 VALUES(33,44);
/*ordered*/ SELECT f1 AS x FROM test1 ORDER BY -x;
SELECT f1-22 AS x, f2-22 AS y FROM test1 WHERE x>0 AND y<50;
SELECT f1 AS f2 FROM test1 WHERE f2 = 22;
SELECT f1 + 1 AS g FROM test1 GROUP BY g % 2 HAVING g > 0;
/*ordered*/ SELECT f1 FROM test1 UNION SELECT f2 FROM test1 ORDER BY f2;
/*ordered*/ SELECT f1 FROM test1 UNION SELECT f2+100 FROM test1 ORDER BY f2+100 DESC;
SELECT f1 FROM test1 UNION SELECT f2+100 FROM test1 ORDER BY f2+101;
/*ordered*/ SELECT f1 AS q FROM test1 UNION SELECT f2 FROM test1 ORDER BY q DESC;
/*ordered*/ SELECT f1 FROM test1 UNION ALL SELECT f2 FROM test1 ORDER BY f1 COLLATE nocase DESC, 1;
SELECT f1 FROM test1 UNION ALL SELECT f2 FROM test1 ORDER BY 2;
SELECT f1 FROM test1 ORDER BY 3;
SELECT * FROM test1 UNION ALL SELECT f2, f1 FROM test1 ORDER BY nosuch;
WITH RECURSIVE t(n, p) AS (SELECT 1, NULL UNION ALL SELECT n + 1, n FROM t WHERE n < 6 ORDER BY p) SELECT n FROM t;
CREATE TABLE t2(y, z);
INSERT INTO t2 VALUES(2,3);
INSERT INTO t2 VALUES(4,5);
CREATE INDEX t2y ON t2(y);
SELECT * FROM (SELECT * FROM t2 WHERE y=2);
SELECT * FROM t2 WHERE y=2 UNION ALL SELECT 5,6;
/*ordered*/ SELECT * FROM t2 WHERE y=2 UNION ALL SELECT * FROM t2 WHERE y=4 ORDER BY y, z LIMIT 4;
SELECT count(*) FROM (SELECT * FROM t2 WHERE y=4);
CREATE TABLE t3(a, b);
INSERT INTO t3 VALUES(1,2);
CREATE TABLE t4(a, b);
INSERT INTO t4 VALUES(3,4);
SELECT t3.* FROM t3, (SELECT max(a), max(b) FROM t4);
SELECT t3.* FROM (SELECT max(a), max(b) FROM t4), t3;
CREATE TABLE tq(a, r);
INSERT INTO tq VALUES(1,2);
CREATE TABLE tr(x UNIQUE);
INSERT INTO tr VALUES(1);
CREATE VIEW v1a(z, y) AS SELECT x IS NULL, x FROM tr;
SELECT a, a+1 AS b FROM tq LEFT JOIN tr ON x+1=b;
SELECT a, (+a)b FROM tq LEFT JOIN v1a ON y=b;
SELECT f1 as 'f1' FROM test1 ORDER BY f2;
SELECT hi.f1 FROM test1 as 'hi';
SELECT f1 'x' FROM test1;
SELECT test1.f1 "x", test1.f2 'y' FROM test1;
SELECT * FROM (SELECT 1) AS 'sq';
SELECT min(*) FROM test1;
SELECT count(*) FROM test1;
SELECT XYZZY(f1) FROM test1;
SELECT XyZ(f1) FROM test1 WHERE 0;
SELECT SUM(min(f1)) FROM test1;
SELECT f1 FROM test1 WHERE min(f1) > 0;
SELECT f1 FROM test1 ORDER BY min(f1);
SELECT max(f1) FROM test1 ORDER BY min(f1);
SELECT min(f1) AS m FROM test1 GROUP BY f1 HAVING max(m+5)<10;
SELECT min(f1) AS m FROM test1 GROUP BY f1 HAVING m < 20;
SELECT count(f1) AS cn FROM test1 WHERE cn > 0;
-- upsert5 / upsert4 / keyword1: several ON CONFLICT clauses (tried in
-- clause order, the targetless one last); COLLATE inside an expression
-- target; fallback keywords as names and type names; string-literal
-- column names.
CREATE TABLE up1(a INTEGER PRIMARY KEY, b, c UNIQUE, d UNIQUE, e UNIQUE);
INSERT INTO up1 VALUES(1,2,3,4,5);
INSERT INTO up1(a,b,c,d,e) VALUES(1,NULL,3,4,5) ON CONFLICT(a) DO UPDATE SET b='a' ON CONFLICT(c) DO UPDATE SET b='c' ON CONFLICT(d) DO UPDATE SET b='d';
SELECT * FROM up1;
INSERT INTO up1(a,b,c,d,e) VALUES(91,NULL,3,4,5) ON CONFLICT(a) DO UPDATE SET b='a' ON CONFLICT(c) DO UPDATE SET b='c';
SELECT * FROM up1;
INSERT INTO up1(a,b,c,d,e) VALUES(92,NULL,93,4,5) ON CONFLICT(c) DO UPDATE SET b='c' ON CONFLICT DO UPDATE SET b='any';
SELECT * FROM up1;
INSERT INTO up1 VALUES(1,0,0,0,0) ON CONFLICT DO NOTHING ON CONFLICT(c) DO NOTHING;
CREATE TABLE up2(a INTEGER PRIMARY KEY, x, y);
CREATE UNIQUE INDEX up2x ON up2(('x' || x) COLLATE nocase);
INSERT INTO up2 VALUES(1, 'one', 'two');
INSERT INTO up2 VALUES(2, 'one', NULL) ON CONFLICT ('x' || x) DO NOTHING;
INSERT INTO up2 VALUES(2, 'one', NULL) ON CONFLICT (('x' || x) COLLATE binary) DO NOTHING;
INSERT INTO up2 VALUES(2, 'one', NULL) ON CONFLICT ((('x' || x))) DO NOTHING;
SELECT * FROM up2;
CREATE TABLE abort(abort abort);
INSERT INTO abort VALUES(99);
SELECT * FROM abort ORDER BY abort ASC;
CREATE TABLE cast(cast cast, like like, pragma INT);
INSERT INTO cast VALUES(1, 2, 3);
SELECT like, pragma FROM cast;
CREATE TABLE sq(w, x INTEGER, 'a b', z, PRIMARY KEY(x, 'a b'));
INSERT INTO sq VALUES(1, 2, 3, 4);
SELECT "a b", typeof(x) FROM sq;
CREATE INDEX sqi ON sq('a b');
