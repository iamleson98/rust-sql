-- A WITHOUT ROWID scan walks PRIMARY KEY order: ORDER BY a prefix of the key in
-- its declared directions and collations sorts nothing, so a LIMIT stops the
-- scan as in SQLite — `WHERE abs(l) ORDER BY j LIMIT 2` never evaluates the
-- i64::MIN row (it raised integer overflow; strict-fuzz seed 675930).
CREATE TABLE t3(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
INSERT INTO t3 VALUES ('a', 1, 1), ('b', 2, 2), ('c', 3, -9223372036854775808), ('d', 4, 4), ('B', 5, 5);
/*ordered*/ SELECT j FROM t3 WHERE abs(l) ORDER BY j LIMIT 2;
/*ordered*/ SELECT j FROM t3 ORDER BY j;
/*ordered*/ SELECT j FROM t3 ORDER BY j COLLATE NOCASE, k;
/*ordered*/ SELECT j FROM t3 ORDER BY j DESC LIMIT 2;
/*ordered*/ SELECT j, k FROM t3 WHERE k < 5 ORDER BY t3.j;
/*ordered*/ SELECT x.j FROM t3 AS x ORDER BY x.j LIMIT 3;
CREATE TABLE m(a INTEGER, b TEXT COLLATE NOCASE, c, PRIMARY KEY (a DESC, b)) WITHOUT ROWID;
INSERT INTO m VALUES (1, 'x', 1), (1, 'Y', 2), (2, 'a', 3), (3, 'b', 4), (2, 'B', 5);
/*ordered*/ SELECT a, b FROM m ORDER BY a DESC;
/*ordered*/ SELECT a, b FROM m ORDER BY a DESC, b;
/*ordered*/ SELECT a, b FROM m ORDER BY a DESC, b COLLATE BINARY;
/*ordered*/ SELECT a, b FROM m ORDER BY a DESC, b DESC;
/*ordered*/ SELECT a, b FROM m ORDER BY a DESC, b, c LIMIT 3;
CREATE TABLE n(p TEXT, q INT, PRIMARY KEY (p COLLATE NOCASE)) WITHOUT ROWID;
INSERT INTO n VALUES ('b', 1), ('A', 2), ('c', 3);
/*ordered*/ SELECT p FROM n ORDER BY p;
/*ordered*/ SELECT p FROM n ORDER BY p COLLATE NOCASE;
