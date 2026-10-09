-- Join ON comparisons between two COLUMNS apply SQLite's comparison affinity
-- (a numeric-affinity column converts a TEXT / BLOB / untyped one): the
-- compiled join term compared raw values (LEFT JOIN ON (text_col > real_col)
-- matched rows SQLite does not).
CREATE TABLE t1(a INTEGER, b TEXT, c REAL, f);
INSERT INTO t1 VALUES ('5.', -2, NULL, 4), (NULL, NULL, 2, 1), (1, 5, 3, 2), (-1, ' 12', 2, 3), (2, '-0', x'', 5), ('4', '-2', -2.0, 6), ('NULL', -3.25, 0.1, 7), (NULL, -1, '-2', 8), ('x y', '+7', x'3132', 9), (4294967296, x'', NULL, 10), (x'3132', '-4', 1.0, 11);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g);
INSERT INTO t2(g) VALUES (1), (2), (3);
SELECT f, b, c, b > c FROM t1;
SELECT f FROM t1 WHERE b > c;
SELECT t1.f, t2.g FROM t1 LEFT JOIN t2 ON (b > c);
SELECT t1.f, t2.g FROM t1 JOIN t2 ON (b > c);
SELECT t1.f, t2.g FROM t1 LEFT JOIN t2 ON (t1.b > t1.c AND t2.g > 1);
SELECT t1.f, t2.g FROM t1 LEFT JOIN t2 ON (t1.b > t2.g);
CREATE TABLE p(n NUMERIC, t TEXT, i INTEGER, x, bl BLOB);
INSERT INTO p VALUES ('10', '10', 10, '10', '10'), (2.5, '2.5', 3, 2.5, x'31'), ('abc', 'abc', 0, 'abc', 'abc'), (NULL, '-1', -1, -1, NULL);
CREATE TABLE q(k INTEGER, s TEXT, r REAL, u);
INSERT INTO q VALUES (10, '10', 10.0, '10'), (3, '3', 2.5, 3), (-1, '-1', -1.0, '-1');
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.t = q.k;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.t = q.r;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.x = q.k;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.n = q.s;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.bl = q.k;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.i > q.s;
SELECT p.rowid, q.rowid FROM p LEFT JOIN q ON p.t < q.u;
SELECT p.rowid, q.rowid FROM p RIGHT JOIN q ON p.t = q.k;
SELECT p.rowid, q.rowid FROM p FULL JOIN q ON p.t >= q.r;
SELECT p.rowid, q.rowid FROM p JOIN q ON p.t = q.k OR p.x = q.s;
