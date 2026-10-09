-- The rowid (an INTEGER PRIMARY KEY column or a rowid spelling) defines no
-- collation: the next operand's collation decides (sqlite3ExprCollSeq skips
-- iColumn -1), e.g. multi-argument max/min over a NOCASE column.
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT COLLATE NOCASE, i REAL);
INSERT INTO t2(g, h, i) VALUES (1, 'abc', 2), (2, 'a%b', 3), (3, 'zzz', 1);
SELECT max('NULL', id, h) FROM t2;
SELECT 1 UNION SELECT max('NULL', id, h) FROM t2;
SELECT 5, 6 UNION SELECT g, max('NULL', id, h) FROM t2;
SELECT max('NULL', id, h) FROM t2 UNION ALL SELECT 1;
SELECT * FROM (SELECT max('NULL', id, h) AS m FROM t2);
SELECT max('NULL', id, h) FROM t2 ORDER BY 1;
SELECT DISTINCT max('NULL', id, h) FROM t2;
SELECT max('NULL', id, h) FROM t2 WHERE g > 0 UNION SELECT 'x';
SELECT max('NULL', rowid, h), min('zzz', oid, h), max('NULL', _rowid_, h) FROM t2;
SELECT id = h, h = id, rowid < h, CASE id WHEN h THEN 1 ELSE 0 END FROM t2;
SELECT max('NULL', id) , max(id, 'NULL'), max(id, h) FROM t2;
CREATE TABLE w(k TEXT PRIMARY KEY COLLATE NOCASE, v) WITHOUT ROWID;
INSERT INTO w VALUES ('abc', 1), ('zzz', 2);
SELECT max('NULL', k), max('NULL', v, k) FROM w;
CREATE TABLE ip(id INTEGER PRIMARY KEY COLLATE NOCASE, h TEXT COLLATE RTRIM);
INSERT INTO ip VALUES (1, 'a  '), (2, 'b');
SELECT max('a', id, h), min('b ', id, h), max('a', h, id) FROM ip;
