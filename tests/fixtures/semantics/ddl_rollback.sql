-- ROLLBACK / ROLLBACK TO undo DDL in the catalog as well as on disk: the
-- rolled-back tables and indexes must vanish (no plan may route through a
-- dropped index's pages; re-creating them must succeed), dropped and
-- altered objects come back, and TEMP objects created before the
-- transaction survive. Savepoint depths count only user savepoints (a
-- BEGIN-started transaction's internal level used to shift them by one).
CREATE TABLE t1(a INTEGER PRIMARY KEY, e TEXT);
INSERT INTO t1(e) VALUES ('v1'),('v2'),('v3'),('v1'),('v4');
CREATE TEMP TABLE keep_me(x);
INSERT INTO keep_me VALUES (1);
BEGIN;
CREATE INDEX ix ON t1(e);
CREATE TABLE z(q);
INSERT INTO z VALUES (1);
DROP TABLE keep_me;
ALTER TABLE t1 ADD COLUMN f INT DEFAULT 7;
ROLLBACK;
SELECT name FROM sqlite_master ORDER BY name;
SELECT count(*) FROM t1 WHERE e = 'v1';
SELECT * FROM keep_me;
SELECT * FROM z;
SELECT * FROM t1 WHERE a = 1;
CREATE INDEX ix ON t1(e);
DROP INDEX ix;
BEGIN;
CREATE TABLE a1(x);
SAVEPOINT s1;
CREATE INDEX ix ON t1(e);
SAVEPOINT s2;
CREATE TABLE a2(y);
INSERT INTO a2 VALUES (5);
ROLLBACK TO s2;
SELECT name FROM sqlite_master ORDER BY name;
SELECT * FROM a2;
SELECT count(*) FROM t1 WHERE e = 'v1';
ROLLBACK TO s1;
SELECT name FROM sqlite_master ORDER BY name;
SELECT count(*) FROM t1 WHERE e = 'v1';
CREATE INDEX ix ON t1(e);
RELEASE s1;
COMMIT;
SELECT name FROM sqlite_master ORDER BY name;
SELECT count(*) FROM t1 WHERE e = 'v1';
SAVEPOINT outer_sp;
DROP INDEX ix;
CREATE TABLE b1(x);
ROLLBACK TO outer_sp;
RELEASE outer_sp;
SELECT name FROM sqlite_master ORDER BY name;
SELECT count(*) FROM t1 WHERE e = 'v2';
BEGIN;
ALTER TABLE t1 RENAME TO t9;
SELECT count(*) FROM t9;
ROLLBACK;
SELECT count(*) FROM t1;
SELECT * FROM t9;
PRAGMA integrity_check;
