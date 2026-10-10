-- LIKE / GLOB compare a BLOB's bytes as text, and BLOB index keys sort after
-- every TEXT key: an index range built from the pattern's prefix must also
-- visit the BLOB keys with that prefix (SQLite's LIKE optimization runs its
-- loop a second time with BLOB bounds). The text range alone missed
-- `x'3132' LIKE '1%'`.
CREATE TABLE t1(a INTEGER, b TEXT, c REAL);
CREATE INDEX t1_bc ON t1(b, c);
INSERT INTO t1 VALUES (1, '1.0', 2), (2, x'3132', 3), (3, 'abc', 4), (4, '1x', 5), (5, x'41', 6),
  (6, x'31', 7), (7, x'32', 8), (8, '12', 9), (9, x'313233', 10), (10, NULL, 11);
SELECT a FROM t1 WHERE b LIKE '1%' ORDER BY a;
SELECT a FROM t1 WHERE b GLOB '1*' ORDER BY a;
SELECT a FROM t1 WHERE b LIKE '12' ORDER BY a;
SELECT a FROM t1 WHERE b GLOB '12' ORDER BY a;
SELECT a FROM t1 WHERE b LIKE '12%' AND c > 3 ORDER BY a;
SELECT count(*) FROM t1 WHERE b LIKE '1%';
SELECT b FROM t1 WHERE b GLOB '1*' ORDER BY b LIMIT 3;
SELECT a FROM t1 WHERE b LIKE '1%' ORDER BY b DESC, a LIMIT 2;
CREATE TABLE n(a INTEGER, h TEXT COLLATE NOCASE);
CREATE INDEX n_h ON n(h);
INSERT INTO n VALUES (1, 'ab'), (2, x'6162'), (3, x'4142'), (4, 'AB'), (5, 'b');
SELECT a FROM n WHERE h LIKE 'ab%' ORDER BY a;
SELECT a FROM n WHERE h GLOB 'ab*' ORDER BY a;
UPDATE t1 SET c = -1 WHERE b LIKE '12%';
SELECT a, c FROM t1 ORDER BY a;
DELETE FROM t1 WHERE b GLOB '1*';
SELECT a FROM t1 ORDER BY a;
SELECT a FROM n WHERE h LIKE 'ab' ORDER BY a;
SELECT a FROM n WHERE h LIKE 'AB%' ORDER BY a;
DELETE FROM n WHERE h LIKE 'aB%';
SELECT a FROM n ORDER BY a;
INSERT INTO n VALUES (6, x'6162'), (7, x'4162'), (8, 'aB'), (9, x'61'), (10, x'6163');
UPDATE n SET a = a + 100 WHERE h LIKE 'ab%';
SELECT a FROM n ORDER BY a;
SELECT n.a, m.a FROM n AS m JOIN n ON n.h LIKE 'a%' AND m.a = n.a ORDER BY 1;
SELECT count(*) FROM n WHERE h LIKE 'a%' AND a > 100;
SELECT a FROM n WHERE h LIKE 'a%' ORDER BY h, a;
SELECT a FROM n WHERE h GLOB 'a*' ORDER BY a;
SELECT (SELECT count(*) FROM n AS i WHERE i.h LIKE 'a%' AND i.a <= o.a) FROM n AS o ORDER BY o.a;
