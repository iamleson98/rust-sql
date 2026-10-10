-- Deleting a table's max-rowid row frees that rowid for an AFTER DELETE trigger
-- inserting into the same table: the next automatic rowid builds on the
-- REMAINING max (OP_NewRowid reads the tree's last rowid), per deleted row —
-- the cached max used to stay at the deleted one (stateful seed 76076).
CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
CREATE TRIGGER trg3 AFTER DELETE ON audit BEGIN INSERT INTO audit (note) VALUES ('p'); END;
INSERT INTO audit VALUES (1, 'a'), (9007199254740992, 'b');
DELETE FROM audit WHERE id IN (-5, 9007199254740992, x'00');
SELECT rowid, note FROM audit ORDER BY 1;
CREATE TABLE a2 (id INTEGER PRIMARY KEY, note TEXT);
CREATE TRIGGER trg4 AFTER DELETE ON a2 BEGIN INSERT INTO a2 (note) VALUES ('p'); END;
INSERT INTO a2 VALUES (1, 'a'), (100, 'b');
DELETE FROM a2 WHERE id = 100;
SELECT rowid, note FROM a2 ORDER BY 1;
INSERT INTO a2 VALUES (200, 'c');
DELETE FROM a2 WHERE id IN (200);
SELECT rowid, note FROM a2 ORDER BY 1;
INSERT INTO a2 VALUES (300, 'c');
DELETE FROM a2 WHERE id >= 300;
SELECT rowid, note FROM a2 ORDER BY 1;
CREATE TABLE a3 (id INTEGER PRIMARY KEY, note TEXT);
CREATE TRIGGER trg5 AFTER DELETE ON a3 WHEN old.note <> 'p' BEGIN INSERT INTO a3 (note) VALUES ('p'); END;
INSERT INTO a3 VALUES (1, 'a'), (50, 'b'), (100, 'c');
DELETE FROM a3 WHERE id >= 50;
SELECT rowid, note FROM a3 ORDER BY 1;
DELETE FROM a3 WHERE note <> 'zz';
SELECT rowid, note FROM a3 ORDER BY 1;
CREATE TABLE a4 (id INTEGER PRIMARY KEY, note TEXT);
CREATE TRIGGER trg6 BEFORE DELETE ON a4 WHEN old.note <> 'p' BEGIN INSERT INTO a4 (note) VALUES ('p'); END;
INSERT INTO a4 VALUES (1, 'a'), (50, 'b'), (100, 'c');
DELETE FROM a4 WHERE id >= 50;
SELECT rowid, note FROM a4 ORDER BY 1;
