-- AUTOINCREMENT follows SQLite's register model (autoIncStep /
-- sqlite3AutoincrementEnd): every rowid an INSERT computes raises the
-- high-water BEFORE constraint checks (a row OR IGNORE drops still burns its
-- id), the sequence is written once when the statement completes, and a
-- statement halted by FAIL / ABORT / ROLLBACK writes nothing — even the rows
-- FAIL keeps leave it alone. Trigger programs share the top-level register
-- (stateful seed 74074).
CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT, u TEXT UNIQUE, n TEXT NOT NULL ON CONFLICT IGNORE);
INSERT INTO t (u, n) VALUES ('x', 'n');
INSERT OR IGNORE INTO t (u, n) VALUES ('x', 'n');
SELECT seq FROM sqlite_sequence;
INSERT INTO t (u, n) VALUES ('y', 'n');
SELECT a FROM t ORDER BY a;
INSERT INTO t (a, u, n) VALUES (50, 'z', NULL);
SELECT seq FROM sqlite_sequence;
INSERT OR IGNORE INTO t (a, u, n) VALUES (60, 'x', 'n'), (70, 'w', 'n');
SELECT seq FROM sqlite_sequence;
INSERT OR FAIL INTO t (a, u, n) VALUES (80, 'v', 'n'), (90, 'x', 'n');
SELECT seq FROM sqlite_sequence;
SELECT a FROM t ORDER BY a;
BEGIN;
INSERT OR ROLLBACK INTO t (a, u, n) VALUES (100, 'q', 'n'), (110, 'x', 'n');
SELECT seq FROM sqlite_sequence;
INSERT INTO t (a, u, n) VALUES (120, 'x', 'n');
SELECT seq FROM sqlite_sequence;
INSERT OR REPLACE INTO t (a, u, n) VALUES (5, 'x', 'n');
SELECT seq FROM sqlite_sequence;
SELECT a, u FROM t ORDER BY a;
CREATE TABLE p (a INTEGER PRIMARY KEY AUTOINCREMENT, u TEXT UNIQUE);
CREATE TABLE log (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT);
CREATE TRIGGER pd AFTER INSERT ON p WHEN NEW.u = 'del' BEGIN DELETE FROM p WHERE a = NEW.a; INSERT INTO p (u) VALUES ('after'); END;
INSERT INTO p (u) VALUES ('a'), ('b');
INSERT INTO p (a, u) VALUES (10, 'del');
SELECT a, u FROM p ORDER BY a;
SELECT name, seq FROM sqlite_sequence ORDER BY name;
INSERT INTO p (rowid, u) VALUES (500, 'r');
INSERT INTO p (a, u) VALUES (600, 'a') ON CONFLICT(u) DO UPDATE SET u = 'a2';
INSERT INTO p (u) VALUES ('a2') ON CONFLICT(u) DO NOTHING;
SELECT name, seq FROM sqlite_sequence ORDER BY name;
INSERT OR IGNORE INTO p (u) SELECT u FROM p;
SELECT name, seq FROM sqlite_sequence ORDER BY name;
CREATE TRIGGER pl AFTER INSERT ON p BEGIN INSERT INTO log (msg) VALUES (NEW.u); END;
INSERT OR IGNORE INTO p (u) VALUES ('new1'), ('a2'), ('new2');
SELECT name, seq FROM sqlite_sequence ORDER BY name;
INSERT OR FAIL INTO p (a, u) VALUES (5000, 'f1'), (5001, 'a2');
SELECT name, seq FROM sqlite_sequence ORDER BY name;
INSERT INTO p (u) VALUES ('next');
SELECT name, seq FROM sqlite_sequence ORDER BY name;
SELECT id, msg FROM log ORDER BY id;
