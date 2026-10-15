-- Multi-row INSERT / REPLACE ... SELECT (above the sorted-batch row
-- threshold) into tables with UNIQUE indexes: duplicates inside the
-- statement's own rows are detected, an ABORT undoes the whole
-- statement, REPLACE resolves conflicts against pre-existing rows AND
-- the statement's earlier rows (the later row wins), and every index
-- agrees with the table afterwards.
CREATE TABLE src(i INTEGER PRIMARY KEY, a, b, c);
WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 300)
INSERT INTO src SELECT x, (x * 37) % 211, x % 97, printf('v%03d', x % 50) FROM n;
UPDATE src SET a = NULL WHERE i % 41 = 0;
CREATE TABLE u(a UNIQUE, b INTEGER, c TEXT COLLATE NOCASE UNIQUE);
CREATE INDEX ub ON u(b);
-- ABORT: the 212th row repeats an `a` — nothing of the statement stays.
BEGIN;
INSERT INTO u(a, b, c) VALUES (-1, -1, 'keep');
INSERT INTO u(a, b, c) SELECT a, b, 'r' || i FROM src;
COMMIT;
SELECT count(*), sum(b) FROM u;
-- Duplicates against the statement's own rows under NOCASE.
INSERT INTO u(a, b, c) SELECT i + 1000, b, 'X' || (i % 70) FROM src WHERE i <= 69;
INSERT INTO u(a, b, c) SELECT i + 2000, b, 'x' || (i % 70) FROM src WHERE i > 230;
SELECT count(*) FROM u;
-- REPLACE: conflicts on `a` (pre-existing and in-statement) and on `c`.
REPLACE INTO u(a, b, c) SELECT a, i, 'r' || (i % 120) FROM src;
/*ordered*/ SELECT a, b, c FROM u ORDER BY a, c;
SELECT count(*), count(DISTINCT a), count(DISTINCT c COLLATE NOCASE) FROM u;
/*ordered*/ SELECT b, count(*) FROM u INDEXED BY ub WHERE b BETWEEN 10 AND 20 GROUP BY b;
/*ordered*/ SELECT a FROM u WHERE a IN (1, 37, 74, 210) ORDER BY a;
/*ordered*/ SELECT c FROM u WHERE c IN ('R1', 'r119', 'X5') ORDER BY c;
PRAGMA integrity_check;
-- INTEGER PRIMARY KEY REPLACE inside one statement: the same rowid twice.
CREATE TABLE p(id INTEGER PRIMARY KEY, k UNIQUE, v);
CREATE INDEX pv ON p(v);
INSERT INTO p SELECT i, i, i FROM src WHERE i <= 100;
REPLACE INTO p SELECT i % 80, i + 500, i % 7 FROM src;
/*ordered*/ SELECT id, k, v FROM p ORDER BY id;
/*ordered*/ SELECT v, count(*) FROM p INDEXED BY pv GROUP BY v;
SELECT count(*) FROM p WHERE k < 500;
PRAGMA integrity_check;
-- Second REPLACE over a full table: every row conflicts.
REPLACE INTO p SELECT i % 80, i + 500, -i FROM src;
/*ordered*/ SELECT id, k, v FROM p ORDER BY id LIMIT 12;
SELECT count(*), sum(v) FROM p INDEXED BY pv WHERE v < 0;
PRAGMA integrity_check;
