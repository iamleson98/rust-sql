-- A transaction's savepoints all end with it. SAVEPOINT a; ROLLBACK left
-- the per-savepoint map snapshot behind, and a later ROLLBACK TO restored
-- it: its old table root and max rowid hid the rows committed in between
-- and the next INSERT reused their rowids (stateful seed 878878). The
-- savepoint-started flag also survived, so RELEASE of a savepoint inside
-- a later BEGIN transaction committed it.
CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
INSERT INTO audit (id, note) VALUES (-49, 'a'), (56, 'b'), (NULL, 'c');
SAVEPOINT sp1;
ROLLBACK;
INSERT INTO audit (note) VALUES (NULL), ('d'), ('e'), ('f');
SAVEPOINT sp1;
ROLLBACK TO sp1;
INSERT INTO audit (note) VALUES ('g'), ('h'), ('i');
RELEASE sp1;
/*ordered*/ SELECT id, note FROM audit ORDER BY rowid;
PRAGMA integrity_check;
SAVEPOINT s2;
INSERT INTO audit (note) VALUES ('j');
COMMIT;
SAVEPOINT s3;
ROLLBACK TO s3;
RELEASE s3;
/*ordered*/ SELECT id, note FROM audit ORDER BY rowid;
SAVEPOINT s4;
ROLLBACK;
BEGIN;
SAVEPOINT s5;
INSERT INTO audit (note) VALUES ('k');
RELEASE s5;
ROLLBACK;
/*ordered*/ SELECT id, note FROM audit ORDER BY rowid;
PRAGMA integrity_check;
