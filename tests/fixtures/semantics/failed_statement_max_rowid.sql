-- A failed statement's rows are undone, but the max rowid it computed mid-way
-- survived: the UPDATE moves the max-rowid holder (47 -> 0), its trigger's
-- INSERT rescans the max as 46 and then fails, the undo puts row 47 back,
-- and the kept max of 46 made the next INSERT reuse rowid 47 — a duplicate
-- rowid and orphaned index entries (stateful seed 293293).
CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
CREATE TRIGGER trg0 AFTER UPDATE ON audit BEGIN INSERT INTO audit (note) VALUES (''); END;
INSERT INTO audit (id, note) VALUES (45, 'zeta') ON CONFLICT(id) DO NOTHING;
UPDATE audit SET note = 'delta' WHERE rowid BETWEEN 3 AND 71;
CREATE UNIQUE INDEX ix20 ON audit(note) WHERE note IS NOT NULL;
CREATE TRIGGER trg22 AFTER INSERT ON audit BEGIN INSERT INTO audit (note) VALUES ('NULL'); END;
INSERT OR IGNORE INTO audit (id, note) VALUES (32, NULL);
INSERT INTO audit (note) VALUES (NULL);
UPDATE audit SET id = 0, note = 'gamma' WHERE note = 'NULL';
DROP INDEX ix20;
INSERT INTO audit (note) VALUES ('delta'), ('line break'), ('alpha'), ('delta');
SELECT id, note FROM audit ORDER BY id;
PRAGMA integrity_check;
