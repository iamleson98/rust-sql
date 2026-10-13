-- Found by SQLite's TCL test suite (changes.test, check.test):
-- * a CTE may recurse without the RECURSIVE keyword;
-- * PRAGMA journal_mode reports the requested rollback mode (OFF stayed
--   "delete"; :memory: keeps only memory / off);
-- * a dangling `CONSTRAINT name` and comma-less table constraints parse;
-- * subqueries are prohibited in CHECK / generated columns / index
--   expressions / partial-index WHERE;
-- * the double-quoted-string fallback: a "name" that resolves to no
--   column is the string 'name' (queries, DML, CHECK, DEFAULT, views,
--   trigger bodies).
CREATE TABLE t1(x INTEGER PRIMARY KEY);
WITH s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 5) SELECT * FROM s;
WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 5) INSERT INTO t1 SELECT i FROM s;
SELECT count(*) FROM t1;
WITH s(i) AS (SELECT 10 UNION ALL SELECT i+1 FROM s WHERE i < 15) INSERT INTO t1 SELECT i FROM s;
SELECT count(*) FROM t1;
WITH a(x) AS (SELECT 1), b(y) AS (SELECT x+1 FROM a UNION ALL SELECT y+1 FROM b WHERE y<4) SELECT * FROM b;
PRAGMA journal_mode;
PRAGMA journal_mode=off;
PRAGMA journal_mode;
PRAGMA journal_mode=memory;
PRAGMA journal_mode=delete;
PRAGMA journal_mode=truncate;
PRAGMA journal_mode=persist;
WITH t AS (SELECT a+10 AS a FROM t) SELECT * FROM t;
WITH RECURSIVE t AS (SELECT a+10 AS a FROM t) SELECT * FROM t;
WITH t(a) AS (SELECT 5 UNION ALL SELECT a+1 FROM t WHERE a<7) SELECT * FROM t;
WITH x AS (SELECT 1 UNION SELECT * FROM x) SELECT * FROM x;
WITH x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<3), y AS (SELECT * FROM x) SELECT * FROM y;
CREATE TABLE t2b(x INTEGER CHECK( typeof(coalesce(x,0))=='integer' ) CONSTRAINT one, y TEXT PRIMARY KEY constraint two, z INTEGER, UNIQUE(x,z) constraint three);
INSERT INTO t2b VALUES('xyzzy','hi',5);
CREATE TABLE t2c(x INTEGER CONSTRAINT x_one CONSTRAINT x_two CHECK( typeof(coalesce(x,0))=='integer' ) CONSTRAINT x_two CONSTRAINT x_three, y INTEGER, z INTEGER, CONSTRAINT u_one UNIQUE(x,y,z) CONSTRAINT u_two);
INSERT INTO t2c VALUES('xyzzy',7,8);
CREATE TABLE t3(x, y, z, CHECK( x<(SELECT min(x) FROM t1) ));
CREATE TABLE t4(x, y CHECK(y IN (SELECT 1)));
CREATE TABLE t5(a, b AS ((SELECT 1)));
CREATE TABLE t6(a,b);
CREATE INDEX i6 ON t6((SELECT 1));
CREATE INDEX i7 ON t6(a) WHERE b IN (SELECT 1);
CREATE TABLE t7(a, b, CHECK(a>0) CHECK(b>0) UNIQUE(a) PRIMARY KEY(b));
INSERT INTO t7 VALUES(1, -1);
SELECT sql FROM sqlite_master WHERE name='t7';
CREATE TABLE t8(a, CONSTRAINT c1);
CREATE TABLE t9(a, CONSTRAINT c1 CHECK(a<5), CONSTRAINT c2);
INSERT INTO t9 VALUES(9);
CREATE TABLE IF NOT EXISTS t(a);
INSERT INTO t VALUES(1);
SELECT "abc", "a" FROM t;
SELECT * FROM t WHERE "xyz" = 'xyz';
CREATE TABLE t2(x CHECK(x != "bad"));
INSERT INTO t2 VALUES('bad');
CREATE TABLE t3(x DEFAULT "dflt");
INSERT INTO t3 DEFAULT VALUES;
SELECT * FROM t3;
CREATE INDEX i1 ON t(a, "zz");
INSERT INTO t VALUES(1, 'x');
CREATE VIEW v AS SELECT a, "lit" AS c FROM t WHERE b = "x";
SELECT * FROM v;
CREATE TABLE log(m);
CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES("hello"); END;
INSERT INTO t VALUES(2, 'y');
SELECT * FROM log;
UPDATE t SET b = "zz" WHERE a = 2;
SELECT * FROM t;
SELECT "a" + 1, "t"."a" FROM t;
SELECT "nope".a FROM t;
CREATE TABLE c(x CHECK(x <> "bad"), y DEFAULT "d" CHECK (y != "y"));
INSERT INTO c VALUES('bad', 1);
INSERT INTO c VALUES('ok', 'y');
INSERT INTO c(x) VALUES('ok2');
SELECT * FROM c;
SELECT sql FROM sqlite_master WHERE name='c';
-- System tables (autoinc.test): sqlite_% tables may not be indexed / dropped
-- (but sqlite_stat*) / altered; every CREATE name sqlite_% is reserved.
CREATE TABLE sys_t1(x INTEGER PRIMARY KEY AUTOINCREMENT, y);
CREATE INDEX seqidx ON sqlite_sequence(name);
DROP TABLE sqlite_sequence;
ALTER TABLE sqlite_sequence RENAME TO s2;
ALTER TABLE sqlite_sequence ADD COLUMN z;
CREATE VIEW v AS SELECT 1;
CREATE INDEX vi ON v(x);
DROP TABLE sqlite_master;
CREATE INDEX mi ON sqlite_master(name);
ANALYZE;
DROP TABLE sqlite_stat1;
CREATE TABLE sqlite_foo(a);
CREATE INDEX sqlite_autoindex_x ON sys_t1(y);
