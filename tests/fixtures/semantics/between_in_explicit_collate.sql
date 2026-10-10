-- BETWEEN is two comparisons, each with its own collation
-- (sqlite3BinaryCompareCollSeq per pair): an explicit COLLATE on a bound
-- wins over the column's declared collation for THAT comparison only.
-- `x IN (c)` with ONE constant member is parsed as `x == +c`, so the
-- member's explicit COLLATE wins too; with two or more members the left
-- operand's collation decides. Plans must not change the answer: a NOCASE
-- index cannot serve a BINARY bound (strict-fuzz seed 641910).
CREATE TABLE t2(id INTEGER PRIMARY KEY, h TEXT COLLATE NOCASE, i REAL);
INSERT INTO t2(h, i) VALUES ('abc', 1), ('ABC', 2), ('Abc', 3), ('ab', 4), ('B', 5), ('a', 6), ('abd', 7), ('0', 8), ('zz', 9);
CREATE TABLE t1(c REAL);
INSERT INTO t1 VALUES (-1);
SELECT i FROM t2 WHERE h BETWEEN 'a' AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h BETWEEN ('a' COLLATE BINARY) AND 'abc' ORDER BY i;
SELECT i FROM t2 WHERE h NOT BETWEEN '32' AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT i, h BETWEEN '32' AND ('Abc' COLLATE BINARY) FROM t2 ORDER BY i;
SELECT i FROM t1 CROSS JOIN t2 WHERE h BETWEEN (32.71 || c) AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t1 JOIN t2 ON h BETWEEN c AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h BETWEEN (SELECT 32.71 || c FROM t1) AND 'Abc' COLLATE BINARY ORDER BY i;
SELECT i FROM t2 WHERE h IN ('abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h NOT IN ('abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h IN ('abc' COLLATE BINARY, 'q') ORDER BY i;
SELECT i FROM t2 WHERE h IN ('q', 'abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h COLLATE BINARY IN ('abc', 'q') ORDER BY i;
SELECT i, h IN ('abc' COLLATE BINARY), h IN ('abc' COLLATE BINARY, 'q') FROM t2 ORDER BY i;
CREATE TABLE t3(s TEXT);
INSERT INTO t3 VALUES ('abc'), ('ABC');
SELECT s FROM t3 WHERE s IN ('ABC' COLLATE NOCASE) ORDER BY s;
SELECT s FROM t3 WHERE s IN ('ABC' COLLATE NOCASE, 'zz') ORDER BY s;
CREATE INDEX t2_h ON t2(h);
CREATE INDEX t3_s ON t3(s);
SELECT i FROM t2 WHERE h BETWEEN 'a' AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h IN ('abc' COLLATE BINARY) ORDER BY i;
SELECT i FROM t2 WHERE h IN ('abc' COLLATE BINARY, 'q') ORDER BY i;
SELECT i FROM t1 CROSS JOIN t2 WHERE h BETWEEN (32.71 || c) AND ('Abc' COLLATE BINARY) ORDER BY i;
SELECT s FROM t3 WHERE s IN ('ABC' COLLATE NOCASE) ORDER BY s;
SELECT s FROM t3 WHERE s IN ('ABC' COLLATE NOCASE, 'zz') ORDER BY s;
