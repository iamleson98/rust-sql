-- CREATE TABLE ... AS SELECT types each column the way SQLite's
-- sqlite3ResultSetOfSelect does — the expression's affinity (later
-- compound arms when the first has none, BLOB when arms mix text and
-- numbers, none = BLOB) — and stores createTableStmt's DDL: "" / TEXT /
-- NUM / INT / REAL, identifiers quoted only when needed, one line when
-- short. The types used to be guessed from the first row's values (a
-- compound view's INT|TEXT column became INTEGER and joined '1' = 1).
CREATE TABLE m(i INT, t TEXT, r REAL, n NUMERIC, b BLOB, u, c TEXT COLLATE NOCASE, d DATE, v VARCHAR(10));
INSERT INTO m VALUES (1,'a',1.5,2,x'00','u','c','2020','vv');
CREATE TABLE c1 AS SELECT * FROM m;
SELECT sql FROM sqlite_master WHERE name='c1';
CREATE TABLE c2 AS SELECT i+1 AS a, t||'' AS b, CAST(t AS INTEGER) AS c, CAST(i AS REAL) AS d, CAST(i AS NUMERIC) AS e, CAST(i AS TEXT) AS f, NULL AS g, 5 AS h, 'x' AS k, max(i) AS l FROM m;
SELECT sql FROM sqlite_master WHERE name='c2';
CREATE TABLE c3 AS SELECT i FROM m UNION SELECT t FROM m;
SELECT sql FROM sqlite_master WHERE name='c3';
CREATE TABLE c4 AS SELECT 1 AS x UNION SELECT i FROM m;
SELECT sql FROM sqlite_master WHERE name='c4';
CREATE TABLE c5 AS SELECT d, v, c FROM m;
SELECT sql FROM sqlite_master WHERE name='c5';
SELECT typeof(a), typeof(b), typeof(c), typeof(d), typeof(e), typeof(f), typeof(g), typeof(h), typeof(k), typeof(l) FROM c2;
CREATE TABLE c6 AS SELECT x FROM (SELECT i AS x FROM m UNION ALL SELECT 'a');
SELECT sql FROM sqlite_master WHERE name='c6';
CREATE TABLE c7(p, q);
CREATE TABLE c8 AS SELECT * FROM c7;
SELECT sql FROM sqlite_master WHERE name='c8';
CREATE TABLE c9 AS SELECT CAST(1 AS VARCHAR(3)) AS z, CAST(1 AS DECIMAL(5,2)) AS y;
SELECT sql FROM sqlite_master WHERE name='c9';
CREATE TABLE map_integer (id INT, name);
INSERT INTO map_integer VALUES(1,'a');
CREATE TABLE map_text (id TEXT, name);
INSERT INTO map_text VALUES('4','e');
CREATE TABLE data (id TEXT, name);
INSERT INTO data VALUES(1,'abc');
INSERT INTO data VALUES('4','xyz');
CREATE VIEW idmap as
SELECT * FROM map_integer
UNION SELECT * FROM map_text;
CREATE TABLE mzed AS SELECT * FROM idmap;
PRAGMA automatic_index=ON;
SELECT * FROM data JOIN idmap USING(id);
SELECT * FROM data JOIN mzed USING(id);
PRAGMA automatic_index=OFF;
SELECT * FROM data JOIN idmap USING(id);
SELECT * FROM data JOIN mzed USING(id);
