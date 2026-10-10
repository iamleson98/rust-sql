-- INSERT ... SELECT fires BEFORE and AFTER INSERT triggers per source row,
-- exactly like INSERT ... VALUES (it used to fire none; stateful seed 84084).
CREATE TABLE t (a INTEGER PRIMARY KEY, u TEXT UNIQUE);
CREATE TABLE log (id INTEGER PRIMARY KEY, msg TEXT);
CREATE TRIGGER tb BEFORE INSERT ON t BEGIN INSERT INTO log (msg) VALUES ('before ' || NEW.u); END;
CREATE TRIGGER ta AFTER INSERT ON t BEGIN INSERT INTO log (msg) VALUES (NEW.u); END;
CREATE TABLE src (x);
INSERT INTO src VALUES ('s1'), ('s2'), ('s1');
INSERT OR IGNORE INTO t (u) SELECT x FROM src;
SELECT a, u FROM t ORDER BY a;
SELECT id, msg FROM log ORDER BY id;
INSERT INTO t (u) SELECT 'q' || x FROM src WHERE x = 's2';
SELECT id, msg FROM log ORDER BY id;
-- A trigger landing rows in the target table moves the next automatic rowid.
CREATE TABLE w (id INTEGER PRIMARY KEY, v TEXT);
CREATE TRIGGER wa AFTER INSERT ON w WHEN NEW.v = 'dup' BEGIN INSERT INTO w (v) VALUES ('from trigger'); END;
INSERT INTO w (v) SELECT x FROM (SELECT 'dup' AS x UNION ALL SELECT 'plain');
SELECT id, v FROM w ORDER BY id;
