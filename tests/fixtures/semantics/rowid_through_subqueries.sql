-- A FROM-subquery / CTE names an unaliased rowid term by its spelling
-- (SQLite names those columns before resolving the body), so the outer
-- query's rowid finds it — it read the derived table's absent rowid: NULL,
-- and `WHERE rowid > 1` dropped every row (stateful seed 293293's probe).
-- A view names its columns after resolution: over an INTEGER PRIMARY KEY
-- the column is `id`, and `SELECT rowid FROM view` is no such column.
CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
CREATE TABLE u (a, b);
INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, 'b');
INSERT INTO u VALUES (10, 20), (30, 40);
SELECT rowid FROM (SELECT rowid FROM t ORDER BY rowid);
SELECT group_concat(rowid) FROM (SELECT rowid FROM t ORDER BY rowid);
SELECT rowid, v FROM (SELECT rowid, v FROM t);
SELECT oid FROM (SELECT oid FROM t);
SELECT _rowid_ FROM (SELECT _rowid_ FROM t);
SELECT rowid FROM (SELECT id AS rowid FROM t);
SELECT rowid FROM (SELECT v FROM t);
SELECT rowid FROM (SELECT a FROM u);
SELECT x.rowid FROM (SELECT rowid FROM t) AS x;
SELECT rowid FROM (SELECT rowid, v FROM t) WHERE rowid > 1;
CREATE VIEW vw AS SELECT rowid, v FROM t;
SELECT rowid FROM vw;
CREATE VIEW vw2 AS SELECT v FROM t;
SELECT rowid FROM vw2;
WITH c AS (SELECT rowid FROM t) SELECT rowid FROM c;
CREATE VIEW vq AS SELECT t.rowid, t.v FROM t;
SELECT rowid FROM vq;
SELECT * FROM vq;
CREATE TABLE n (v TEXT);
INSERT INTO n VALUES ('a'), ('b');
CREATE VIEW vn AS SELECT rowid, v FROM n;
SELECT rowid, v FROM vn;
CREATE VIEW vo AS SELECT oid FROM t;
SELECT oid FROM vo;
SELECT id FROM vo;
SELECT rowid FROM (SELECT rowid FROM n) WHERE rowid = 2;
WITH c(x) AS (SELECT rowid FROM t) SELECT x FROM c;
SELECT rowid FROM (SELECT rowid FROM t UNION ALL SELECT rowid FROM n);
SELECT RowId FROM (SELECT RowId FROM t);
