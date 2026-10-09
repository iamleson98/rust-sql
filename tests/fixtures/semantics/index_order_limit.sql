-- ORDER BY over an index's leading columns + LIMIT walks the index in order
-- (backwards for DESC) and stops early; ties come out in index order —
-- (key, rowid) — the order SQLite's index walk returns them in.
CREATE TABLE ev(id INTEGER PRIMARY KEY, ts, name TEXT COLLATE NOCASE, g INTEGER);
WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM n WHERE v < 300)
INSERT INTO ev(ts, name, g) SELECT CASE WHEN v % 37 = 0 THEN NULL WHEN v % 11 = 0 THEN (v % 50) + 0.5 WHEN v % 13 = 0 THEN 'txt' || (v % 7) ELSE v % 50 END, CASE v % 4 WHEN 0 THEN 'Abc' WHEN 1 THEN 'abd' WHEN 2 THEN 'ABE' ELSE NULL END, v % 3 FROM n;
CREATE INDEX ev_ts ON ev(ts);
CREATE INDEX ev_name ON ev(name);
CREATE INDEX ev_g_ts ON ev(g, ts);
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts LIMIT 25;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts DESC LIMIT 25;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts LIMIT 10 OFFSET 40;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts DESC LIMIT 7 OFFSET 280;
/*ordered*/ SELECT id, ts FROM ev WHERE ts > 20 ORDER BY ts LIMIT 15;
/*ordered*/ SELECT id, ts FROM ev WHERE ts >= 20 ORDER BY ts LIMIT 15;
/*ordered*/ SELECT id, ts FROM ev WHERE ts < 20 ORDER BY ts DESC LIMIT 15;
/*ordered*/ SELECT id, ts FROM ev WHERE ts <= 20 ORDER BY ts DESC LIMIT 15;
/*ordered*/ SELECT id, ts FROM ev WHERE ts BETWEEN 10 AND 12 ORDER BY ts LIMIT 100;
/*ordered*/ SELECT id, ts FROM ev WHERE ts BETWEEN 10 AND 12 ORDER BY ts DESC LIMIT 100;
/*ordered*/ SELECT id, ts FROM ev WHERE ts > 45 ORDER BY ts LIMIT 100;
/*ordered*/ SELECT id, ts FROM ev WHERE ts > 'txt' ORDER BY ts LIMIT 100;
/*ordered*/ SELECT id, ts FROM ev WHERE ts > 20 AND g = 1 ORDER BY ts LIMIT 10;
/*ordered*/ SELECT id, ts FROM ev WHERE ts > 20 AND name = 'abc' ORDER BY ts DESC LIMIT 10;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts, id LIMIT 12;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts DESC, id DESC LIMIT 12;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts DESC, rowid DESC LIMIT 12;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts, id DESC LIMIT 12;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts NULLS LAST LIMIT 12;
/*ordered*/ SELECT id, name FROM ev ORDER BY name LIMIT 20;
/*ordered*/ SELECT id, name FROM ev ORDER BY name DESC LIMIT 20;
/*ordered*/ SELECT id, name FROM ev ORDER BY name COLLATE BINARY LIMIT 20;
/*ordered*/ SELECT id, name FROM ev WHERE name > 'ABD' ORDER BY name LIMIT 20;
/*ordered*/ SELECT id, name FROM ev WHERE name >= 'abd' ORDER BY name DESC LIMIT 20;
/*ordered*/ SELECT id, g, ts FROM ev WHERE g > 0 ORDER BY g, ts LIMIT 30;
/*ordered*/ SELECT id, g, ts FROM ev ORDER BY g DESC, ts DESC LIMIT 30;
/*ordered*/ SELECT id, g, ts FROM ev WHERE g < 2 ORDER BY g DESC, ts DESC LIMIT 30 OFFSET 5;
/*ordered*/ SELECT ts, count(*) FROM (SELECT ts FROM ev ORDER BY ts LIMIT 50) GROUP BY ts ORDER BY ts;
/*ordered*/ SELECT e.id, e.ts FROM ev e WHERE e.ts > 30 ORDER BY e.ts LIMIT 5;
/*ordered*/ SELECT id, ts * 2 AS dbl FROM ev WHERE ts > 30 ORDER BY ts LIMIT 5;
/*ordered*/ SELECT id FROM ev WHERE ts > 1000 ORDER BY ts LIMIT 5;
/*ordered*/ SELECT id FROM ev WHERE ts > NULL ORDER BY ts LIMIT 5;
/*ordered*/ SELECT id FROM ev ORDER BY ts LIMIT 0;
DELETE FROM ev WHERE id % 5 = 0;
UPDATE ev SET ts = ts + 1 WHERE id % 7 = 0 AND typeof(ts) = 'integer';
/*ordered*/ SELECT id, ts FROM ev WHERE ts >= 30 ORDER BY ts LIMIT 40;
/*ordered*/ SELECT id, ts FROM ev ORDER BY ts DESC LIMIT 40;
