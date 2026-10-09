-- Evaluation extent: a LIMIT ends the scan at the k-th passing row even when
-- the WHERE does not compile (rows past it are never evaluated, so cannot
-- raise), including ORDER BY rowid / INTEGER PRIMARY KEY (the scan order)
-- and INSERT ... SELECT sources.
CREATE TABLE q(a INTEGER, f INTEGER);
CREATE TABLE p(id INTEGER PRIMARY KEY, f INTEGER);
INSERT INTO q VALUES (1,-1),(2,-2),(3,-3),(4,-9223372036854775808),(5,-5);
INSERT INTO p VALUES (1,-1),(2,-2),(3,-3),(4,-9223372036854775808),(5,-5);
/*ordered*/ SELECT a FROM q WHERE a < abs(f) + 1 ORDER BY rowid LIMIT 3;
/*ordered*/ SELECT a FROM q WHERE a < abs(f) + 1 LIMIT 3;
/*ordered*/ SELECT a FROM q WHERE a < abs(f) + 1 ORDER BY _rowid_ LIMIT 2 OFFSET 1;
/*ordered*/ SELECT id FROM p WHERE id < abs(f) + 1 ORDER BY id LIMIT 3;
/*ordered*/ SELECT id FROM p WHERE id < abs(f) + 1 ORDER BY rowid LIMIT 3;
/*ordered*/ SELECT id FROM p WHERE id < abs(f) + 1 LIMIT 3;
/*ordered*/ SELECT abs(f) FROM q ORDER BY rowid LIMIT 3;
/*ordered*/ SELECT abs(f) FROM q LIMIT 3;
/*ordered*/ SELECT abs(f) FROM p ORDER BY id LIMIT 3;
INSERT INTO q(a) SELECT a + 10 FROM q WHERE a < abs(f) + 1 ORDER BY rowid LIMIT 3;
INSERT INTO q(a) SELECT a + 20 FROM q WHERE a < abs(f) + 1 LIMIT 2;
INSERT INTO p(f) SELECT id FROM p WHERE id < abs(f) + 1 ORDER BY id LIMIT 3;
SELECT * FROM q ORDER BY rowid;
SELECT * FROM p ORDER BY id;
