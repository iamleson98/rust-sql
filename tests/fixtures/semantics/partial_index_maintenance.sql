-- A partial index holds member rows only, on every write path: a
-- rowid-moving UPDATE re-filed the moved row without checking the WHERE
-- predicate, and an upsert's DO UPDATE kept a row that left the index. The
-- stray entry outlived the row once it was deleted (stateful seeds 21021,
-- 67067).
CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
CREATE UNIQUE INDEX ix0 ON audit(note) WHERE note IS NOT NULL;
INSERT INTO audit (id) VALUES (-9007199254740992);
UPDATE audit SET id = -19;
UPDATE audit SET note = 'tab' WHERE note IS NULL;
DELETE FROM audit WHERE note > 111.000;
PRAGMA integrity_check;
INSERT INTO audit (id) VALUES (5);
UPDATE audit SET id = 7;
UPDATE audit SET note = 'x' WHERE note IS NULL;
DELETE FROM audit;
PRAGMA integrity_check;
CREATE TABLE u (id INTEGER PRIMARY KEY, k TEXT UNIQUE, n TEXT);
CREATE INDEX upx ON u(n) WHERE n IS NOT NULL;
INSERT INTO u VALUES (1, 'a', 'x');
INSERT INTO u (k, n) VALUES ('a', 'y') ON CONFLICT(k) DO UPDATE SET n = NULL;
SELECT count(*) FROM u WHERE n IS NOT NULL;
DELETE FROM u;
PRAGMA integrity_check;
INSERT INTO u VALUES (2, 'b', NULL);
INSERT INTO u (k, n) VALUES ('b', 'y') ON CONFLICT(k) DO UPDATE SET n = 'z';
SELECT id, n FROM u WHERE n IS NOT NULL;
DELETE FROM u WHERE n = 'z';
PRAGMA integrity_check;
