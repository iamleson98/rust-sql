-- Rowid / index access-path bounds must never read the scanned row, and
-- redundant bounds must all hold (both were wrong-result planner bugs).
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT);
CREATE INDEX t2g ON t2(g);
INSERT INTO t2(g, h) VALUES (5, 'a'), (1, 'b'), (7, '0'), (2, '9');
SELECT id FROM t2 WHERE (h + 0) < id;
SELECT id FROM t2 WHERE id > (g - 3);
SELECT id FROM t2 WHERE id BETWEEN g - 4 AND g;
SELECT id FROM t2 WHERE id > length(h);
SELECT id FROM t2 WHERE ('Abc' & h) < id;
SELECT id FROM t2 WHERE g > id + 2;
SELECT id FROM t2 WHERE g = id + 1;
SELECT id FROM t2 WHERE g IN (id, id + 1);
SELECT id FROM t2 WHERE id IN (g - 4, 3);
SELECT id FROM t2 WHERE id >= 3 AND id >= 1;
SELECT id FROM t2 WHERE id <= 1 AND id <= 3;
SELECT id FROM t2 WHERE id > 1 AND id > 3;
SELECT id FROM t2 WHERE id BETWEEN 1 AND 3 AND id >= 2;
SELECT id FROM t2 WHERE id = 2 AND id >= 1;
SELECT id FROM t2 WHERE id >= 1 AND id = 9;
SELECT id FROM t2 WHERE id < 4 AND id > 1 AND id <= 2 AND id >= 0;
SELECT id FROM t2 WHERE g >= 5 AND g >= 2;
UPDATE t2 SET h = 'u' WHERE id > g - 3;
SELECT * FROM t2;
DELETE FROM t2 WHERE id < g - 3;
SELECT * FROM t2;
DELETE FROM t2 WHERE id >= 3 AND id >= 1;
SELECT * FROM t2;
-- Index range scans with only an upper bound must skip NULL keys (a
-- DELETE used to remove every NULL row).
CREATE TABLE t1(a INTEGER, b TEXT);
CREATE INDEX t1_a ON t1(a);
CREATE INDEX t1_b ON t1(b);
INSERT INTO t1 VALUES (NULL, NULL), (1, 'a'), (5, 'x'), (NULL, 'b'), (x'00', 'c');
SELECT a FROM t1 WHERE a < 5;
SELECT a FROM t1 WHERE a <= 5;
SELECT a FROM t1 WHERE a < '5';
SELECT a FROM t1 WHERE a < CAST(5 AS TEXT);
SELECT b FROM t1 WHERE b < 'c';
SELECT b FROM t1 WHERE b <= 'b';
SELECT a FROM t1 WHERE a < x'01';
SELECT count(*) FROM t1 WHERE a < 100;
SELECT min(a), max(a), min(b), max(b) FROM t1;
/*ordered*/ SELECT a FROM t1 WHERE a < 9 ORDER BY a;
/*ordered*/ SELECT a FROM t1 WHERE a < 9 ORDER BY a DESC;
/*ordered*/ SELECT b FROM t1 WHERE b < 'z' ORDER BY b DESC LIMIT 2;
DELETE FROM t1 WHERE a < 3;
SELECT * FROM t1;
DELETE FROM t1 WHERE b <= 'b';
SELECT * FROM t1;
-- Subquery-valued bounds: uncorrelated ones are seek values; ones
-- correlated to the scanned row must be evaluated per row.
CREATE TABLE r(id INTEGER PRIMARY KEY, g INTEGER);
INSERT INTO r(g) VALUES (2), (3), (1), (4);
CREATE TABLE u(k INTEGER, x INTEGER);
INSERT INTO u VALUES (2, 1), (3, 2), (1, 9), (4, 4);
SELECT id FROM r WHERE id = (SELECT max(x) FROM u WHERE u.k = r.g);
SELECT id FROM r WHERE id = (SELECT max(x) FROM u WHERE k = g);
SELECT id FROM r WHERE id > (SELECT min(x) FROM u);
SELECT id FROM r WHERE id IN (SELECT x FROM u WHERE u.k = r.g);
SELECT id FROM r WHERE id BETWEEN (SELECT min(x) FROM u WHERE u.k = r.g) AND 4;
UPDATE r SET g = 0 WHERE id = (SELECT max(x) FROM u WHERE u.k = r.g);
SELECT * FROM r;

-- BETWEEN in a scan predicate: Kleene NOT BETWEEN with NULL bounds, and
-- COLUMN bounds (column-to-column affinity; a non-compilable scan
-- predicate used to be dropped — every row matched).
CREATE TABLE nb(j TEXT, k INTEGER, l);
INSERT INTO nb VALUES(' ',4,82),('%',-1,'12 '),('.5',2,-2.25),('abc',NULL,'3'),('z',50,NULL);
SELECT j FROM nb WHERE l <= j;
SELECT j FROM nb WHERE l BETWEEN 0 AND j;
SELECT j FROM nb WHERE k BETWEEN 0 AND l;
SELECT j FROM nb WHERE l BETWEEN k AND 100;
SELECT j FROM nb WHERE l NOT BETWEEN NULL AND j;
SELECT j FROM nb WHERE k NOT BETWEEN NULL AND 3;
SELECT j FROM nb WHERE k NOT BETWEEN 10 AND NULL;
SELECT j FROM nb WHERE NOT (k BETWEEN 10 AND NULL);
SELECT count(*) FROM nb WHERE l BETWEEN 0 AND j;
/*ordered*/ SELECT j, k FROM nb WHERE l BETWEEN 0 AND j ORDER BY k;
-- DML on a WITHOUT ROWID table with a row-dependent bound (used to fail
-- "unsupported: DELETE on a table without INTEGER PRIMARY KEY").
CREATE TABLE wr(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
INSERT INTO wr VALUES(' ',4,82),('5',1,'5'),('x',2,NULL),('abc',NULL,'3'),('%',-1,'12 '),('.5',2,-2.25);
UPDATE wr SET l = 1 WHERE ((k AND '5.') AND (j BETWEEN 0 AND j));
SELECT * FROM wr;
DELETE FROM wr WHERE ((k AND '5.') AND (j BETWEEN 0 AND j));
SELECT * FROM wr;
SELECT j FROM wr WHERE l NOT BETWEEN NULL AND j;
CREATE TABLE wr2(a, b, PRIMARY KEY(a, b)) WITHOUT ROWID;
INSERT INTO wr2 VALUES(1,2),(2,3),(3,1);
UPDATE wr2 SET b = b + 10 WHERE a BETWEEN 1 AND b AND b > 1;
SELECT * FROM wr2;
DELETE FROM wr2 WHERE a < b - 5 RETURNING *;
SELECT * FROM wr2;
CREATE TABLE rd(a INTEGER, b);
CREATE INDEX rd_a ON rd(a);
INSERT INTO rd VALUES(1,1),(2,5),(3,2);
UPDATE rd SET b = 0 WHERE a > b - 2 AND a > 1;
SELECT * FROM rd;
DELETE FROM rd WHERE a >= b AND a > 1;
SELECT * FROM rd;

-- NOT BETWEEN is never an index range (it was planned AS the range and
-- returned exactly the rows it must exclude).
CREATE TABLE nbi(id INTEGER PRIMARY KEY, b TEXT, c INTEGER);
CREATE INDEX nbi_b ON nbi(b);
CREATE INDEX nbi_c ON nbi(c);
INSERT INTO nbi VALUES(1,'-84',1),(2,'3',2),(3,'a_b',3),(4,'0',4),(5,NULL,NULL),(6,'b',6);
SELECT b FROM nbi WHERE b NOT BETWEEN '1' AND 'b';
SELECT b FROM nbi WHERE b NOT BETWEEN 1 AND x'';
SELECT id FROM nbi WHERE c NOT BETWEEN 2 AND 4;
SELECT id FROM nbi WHERE id NOT BETWEEN 2 AND 4;
SELECT id FROM nbi WHERE c NOT BETWEEN 2 AND 4 AND c > 0;
UPDATE nbi SET c = -c WHERE c NOT BETWEEN 2 AND 4;
SELECT * FROM nbi;
DELETE FROM nbi WHERE b NOT BETWEEN '1' AND 'b';
SELECT * FROM nbi;
-- A CAST-typed numeric operand makes the comparison NUMERIC: an index on
-- a TEXT / untyped column cannot serve it (sqlite3IndexAffinityOk).
CREATE TABLE xa(t TEXT, n INTEGER, u);
CREATE INDEX xa_t ON xa(t);
CREATE INDEX xa_n ON xa(n);
CREATE INDEX xa_u ON xa(u);
INSERT INTO xa VALUES('5', 5, '5'),('5.0', 6, 5),('abc', 7, 5.0),('9223372036854775808', 8, 1);
SELECT n FROM xa WHERE t = CAST(5 AS REAL);
SELECT n FROM xa WHERE t = CAST(5 AS INTEGER);
SELECT n FROM xa WHERE t = CAST(5 AS TEXT);
SELECT n FROM xa WHERE n = CAST('5' AS TEXT);
SELECT n FROM xa WHERE u = CAST(5 AS INTEGER);
SELECT n FROM xa WHERE t > CAST(4 AS REAL);
SELECT n FROM xa WHERE t BETWEEN CAST(4 AS REAL) AND CAST(6 AS REAL);
SELECT n FROM xa WHERE CAST(9223372036854775806 AS FLOAT) = t;
-- Index-range residuals on WITHOUT ROWID / rowid-alias tables (a residual
-- used to be DROPPED on WITHOUT ROWID projected ranges, and the lookup
-- path popped the row's last real column — a panic).
CREATE TABLE wrr(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
CREATE INDEX wrr_k ON wrr(k);
INSERT INTO wrr VALUES('-0', 1, 4294967296),('Abc', 0, -5),('z', 3, NULL),('q', 2, 7);
SELECT j, k, l, 1 FROM wrr WHERE k < 5 AND l > 0;
SELECT j, k, l, 1 FROM wrr WHERE k < 5 AND 1;
SELECT j, l FROM wrr WHERE k < 5 AND l > 0;
SELECT l FROM wrr WHERE k BETWEEN 1 AND 3 AND l IS NOT NULL;
SELECT * FROM wrr WHERE k > 0 AND j <> 'q';
CREATE TABLE ipk(id INTEGER PRIMARY KEY, a, b);
CREATE INDEX ipk_a ON ipk(a);
INSERT INTO ipk VALUES(1, 1, 10),(2, 2, -1),(3, 9, 5);
SELECT id, a, b, 1 FROM ipk WHERE a < 5 AND b > 0;
SELECT b FROM ipk WHERE a < 5 AND b > 0;
SELECT a + b FROM ipk WHERE a < 5 AND b > 0;

-- A NULL range bound is an EMPTY range for DML too (it scanned from the
-- first index entry: every row updated / deleted).
CREATE TABLE nbd(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
CREATE INDEX nbd_k ON nbd(k);
INSERT INTO nbd VALUES('a', 1, 1),('b', 2, 9),('c', NULL, 2);
UPDATE nbd SET k = 5 WHERE j > NULL;
SELECT * FROM nbd;
UPDATE nbd SET l = 0 WHERE k < (1 = NULL);
SELECT * FROM nbd;
DELETE FROM nbd WHERE (1 = NULL) < k;
SELECT * FROM nbd;
DELETE FROM nbd WHERE k BETWEEN NULL AND 10;
SELECT * FROM nbd;
-- A subquery over the table being updated / deleted from (copied-out
-- scan; this used to DEADLOCK on the scan's own page lock).
CREATE TABLE sq(id INTEGER PRIMARY KEY, g INTEGER);
INSERT INTO sq(g) VALUES(5),(0),(7),(1);
UPDATE sq SET g = (SELECT count(*) FROM sq WHERE g < 100);
SELECT * FROM sq;
UPDATE sq SET g = (SELECT count(*) FROM sq AS u WHERE u.id < sq.id) WHERE id > 1;
SELECT * FROM sq;
DELETE FROM sq WHERE id > 0 AND g < (SELECT count(*) FROM sq);
SELECT * FROM sq;

-- GLOB character classes are wildcards: no literal prefix range (an
-- index range on the "prefix" `[a-c` found nothing).
CREATE TABLE gl1(j TEXT PRIMARY KEY, k) WITHOUT ROWID;
INSERT INTO gl1 VALUES('a%b',1),('abc',2),('b',3),('d',4),('[x',5);
SELECT j FROM gl1 WHERE j GLOB '[a-c]*';
SELECT j FROM gl1 WHERE j GLOB '[[]*';
SELECT j FROM gl1 WHERE j GLOB 'a?c';
CREATE TABLE gl2(j TEXT, k);
CREATE INDEX gl2_j ON gl2(j);
INSERT INTO gl2 SELECT * FROM gl1;
SELECT j FROM gl2 WHERE j GLOB '[a-c]*';
SELECT j FROM gl2 WHERE j GLOB 'ab[c]';
SELECT j FROM gl2 WHERE j GLOB 'a[%]b';
SELECT j FROM gl2 WHERE j LIKE 'a%';
-- CASE operand comparisons take the WHEN operand's declared collation
-- when the CASE operand is not a column.
CREATE TABLE cc(h TEXT COLLATE NOCASE, g);
INSERT INTO cc VALUES('Abc', 1),('x', 2);
SELECT CASE trim('abc  ') WHEN h THEN 'hit' ELSE 'miss' END, CASE h WHEN 'ABC' THEN 1 ELSE 0 END FROM cc;
-- A raising scalar subquery in an untaken CASE branch never runs.
SELECT CASE WHEN g > 0 THEN 1 ELSE (SELECT max(g) FROM cc WHERE g < abs(-9223372036854775808)) END FROM cc;
SELECT (SELECT 1, 2);
