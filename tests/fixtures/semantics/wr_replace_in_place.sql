-- REPLACE into a WITHOUT ROWID table whose PRIMARY KEY is its only index
-- rewrites the holder in place: same results as delete + insert, the
-- NOCASE key takes the new spelling, payloads may grow into overflow,
-- and a later failing row of the same statement undoes the rewrite.
CREATE TABLE w(a TEXT PRIMARY KEY, b) WITHOUT ROWID;
CREATE TABLE wn(a TEXT COLLATE NOCASE PRIMARY KEY, b) WITHOUT ROWID;
CREATE TABLE wc(p INT, q TEXT, r CHECK (r IS NULL OR r < 100), PRIMARY KEY (p, q)) WITHOUT ROWID;
INSERT INTO w VALUES ('x', 1), ('y', 2), ('z', 3);
REPLACE INTO w VALUES ('y', 20);
REPLACE INTO w VALUES ('x', printf('%.3000c', 'q'));
REPLACE INTO w VALUES ('x', 'short'), ('q', 9), ('z', NULL);
/*ordered*/ SELECT a, length(b), typeof(b) FROM w ORDER BY a;
INSERT INTO wn VALUES ('abc', 1), ('DEF', 2);
REPLACE INTO wn VALUES ('ABC', 10);
/*ordered*/ SELECT a, b FROM wn ORDER BY a;
INSERT INTO wc VALUES (1, 'a', 1), (1, 'b', 2), (2, 'a', 3);
REPLACE INTO wc VALUES (1, 'b', 20);
REPLACE INTO wc VALUES (2, 'a', 30), (1, 'a', 500);
/*ordered*/ SELECT * FROM wc ORDER BY p, q;
SELECT changes(), total_changes() > 0;
CREATE TABLE g (k TEXT PRIMARY KEY ON CONFLICT REPLACE, v) WITHOUT ROWID;
CREATE TABLE r (id INTEGER PRIMARY KEY, v);
INSERT INTO r VALUES (41, 'x');
INSERT INTO g VALUES ('a', 1), ('a', 2), ('b', 3);
SELECT changes(), last_insert_rowid();
INSERT INTO g VALUES ('a', 5);
SELECT changes(), last_insert_rowid();
/*ordered*/ SELECT * FROM g ORDER BY k;
BEGIN;
REPLACE INTO w VALUES ('y', 'in txn');
ROLLBACK;
/*ordered*/ SELECT a, b FROM w WHERE a = 'y';
