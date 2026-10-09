-- Window functions: named windows (WINDOW clause, OVER name, base
-- chaining + override errors), every frame kind, windowed aggregates
-- through the shared aggregate semantics (group_concat separators and
-- value text, TEXT sums), collations in PARTITION BY / ORDER BY.
CREATE TABLE w(id INTEGER PRIMARY KEY, g TEXT, v INTEGER, r REAL, t TEXT COLLATE NOCASE);
INSERT INTO w(g, v, r, t) VALUES('a',1,1.5,'x'),('a',2,NULL,'X'),('a',2,3.0,'y'),('b',5,0.5,'Y'),('b',NULL,2.5,'z'),('c',7,7.0,'x'),('a',9,-1.0,'Z');
/*ordered*/ SELECT id, g, v, sum(v) OVER (PARTITION BY g ORDER BY id) FROM w ORDER BY id;
/*ordered*/ SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY id;
/*ordered*/ SELECT id, sum(v) OVER (ORDER BY v RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY id;
/*ordered*/ SELECT id, avg(v) OVER (PARTITION BY g ORDER BY v GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id;
/*ordered*/ SELECT id, v, rank() OVER (ORDER BY v), dense_rank() OVER (ORDER BY v), percent_rank() OVER (ORDER BY v), cume_dist() OVER (ORDER BY v) FROM w ORDER BY id;
/*ordered*/ SELECT id, ntile(3) OVER (ORDER BY id), lag(v, 2, -1) OVER (ORDER BY id), lead(v) OVER (ORDER BY id) FROM w ORDER BY id;
/*ordered*/ SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY id), last_value(v) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), nth_value(v, 2) OVER (PARTITION BY g ORDER BY id) FROM w ORDER BY id;
/*ordered*/ SELECT id, count(*) OVER w1, max(r) OVER w1 FROM w WINDOW w1 AS (PARTITION BY g) ORDER BY id;
/*ordered*/ SELECT id, count(*) OVER (w1) FROM w WINDOW w1 AS (PARTITION BY g) ORDER BY id;
/*ordered*/ SELECT id, sum(v) OVER (w1 ORDER BY id) FROM w WINDOW w1 AS (PARTITION BY g) ORDER BY id;
/*ordered*/ SELECT id, sum(v) OVER w2 FROM w WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY id DESC) ORDER BY id;
SELECT id, count(*) OVER nope FROM w;
SELECT id, sum(v) OVER (w1 PARTITION BY v) FROM w WINDOW w1 AS (PARTITION BY g);
SELECT id, sum(v) OVER (w1 ORDER BY v) FROM w WINDOW w1 AS (ORDER BY id);
SELECT id, sum(v) OVER (w1) FROM w WINDOW w1 AS (ORDER BY id ROWS 1 PRECEDING);
SELECT id, group_concat(DISTINCT g) OVER (ORDER BY id) FROM w;
/*ordered*/ SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM w ORDER BY id;
/*ordered*/ SELECT id, group_concat(v, '-') OVER (ORDER BY id ROWS 2 PRECEDING) FROM w ORDER BY id;
/*ordered*/ SELECT id, group_concat(v) OVER (ORDER BY id), string_agg(r, ';') OVER (PARTITION BY g) FROM w ORDER BY id;
/*ordered*/ SELECT g, sum(v) FILTER (WHERE v > 1) OVER (PARTITION BY g) FROM w ORDER BY id;
/*ordered*/ SELECT id, row_number() OVER (PARTITION BY g ORDER BY r DESC NULLS LAST) FROM w ORDER BY id;
/*ordered*/ SELECT id, count(*) OVER (PARTITION BY t), dense_rank() OVER (ORDER BY t), rank() OVER (ORDER BY t) FROM w ORDER BY id;
/*ordered*/ SELECT id, min(t) OVER (), max(t) OVER () FROM w ORDER BY id;
/*ordered*/ SELECT id, CASE WHEN row_number() OVER (ORDER BY id) = 1 THEN 'first' ELSE 'rest' END FROM w ORDER BY id;
/*ordered*/ SELECT id, sum(t) OVER (), sum('12abc') OVER (), sum('5') OVER (), typeof(sum('5') OVER ()) FROM w ORDER BY id LIMIT 1;
-- UPSERT + RETURNING (a RETURNING insert after a same-table UPSERT used
-- to return all NULLs).
CREATE TABLE u(k TEXT PRIMARY KEY, n INTEGER DEFAULT 0, note TEXT);
INSERT INTO u VALUES('x', 1, 'a') ON CONFLICT(k) DO UPDATE SET n = n + 1;
INSERT INTO u VALUES('x', 1, 'b') ON CONFLICT(k) DO UPDATE SET n = n + excluded.n, note = excluded.note || note;
INSERT INTO u VALUES('x', 1, 'c') ON CONFLICT(k) DO UPDATE SET n = 100 WHERE n > 50;
INSERT INTO u VALUES('y', 5, 'd') ON CONFLICT DO NOTHING;
INSERT INTO u VALUES('y', 6, 'e') ON CONFLICT DO NOTHING;
SELECT * FROM u;
INSERT INTO u VALUES('z', 3, 'f') RETURNING k, n * 2, upper(note);
UPDATE u SET n = n + 10 WHERE k <> 'z' RETURNING k, n;
DELETE FROM u WHERE k = 'y' RETURNING *;
INSERT OR REPLACE INTO u VALUES('x', 0, 'r');
SELECT * FROM u;
-- JSON: values produced by JSON functions are EMBEDDED as JSON by the
-- constructors (SQLite's JSON subtype), incl. json_group_array.
SELECT json_array(1, 2.5, 'x', NULL, json('[1]')), json_object('k', 1, 'j', json('{"z":2}'));
SELECT json_array(json_extract('{"a":[1]}', '$.a'), json_extract('{"a":"[x"}', '$.a'), json_extract('{"a":"[1]"}', '$.a'), json_extract('{"a":1,"b":2}', '$.a', '$.b'), json_quote('q'));
SELECT json_set('{}', '$.x', json('[1,2]'), '$.y', '[1,2]', '$.z', json_extract('[[3]]', '$[0]'));
SELECT json_array('{"a":1}' -> '$.a', '{"a":[1]}' -> '$.a', '{"a":"s"}' ->> '$.a');
SELECT json_group_array(json_object('g', g, 'v', v)) FROM w WHERE v IS NOT NULL;
SELECT json_group_object(g, json_array(v, r)) FROM w WHERE id < 4;
SELECT json_group_array(json_extract('{"a":[1]}', '$.a')), json_group_array(json_extract('{"a":"[1]"}', '$.a')) FROM w;
SELECT key, value, type, fullkey FROM json_tree('{"x":1,"y":[2,{"z":3}]}');
-- Date / time: field ranges (getDigits limits), the uppercase-only T
-- separator, timezone ranges, and timediff's calendar arithmetic.
SELECT datetime('2024-01-01 25:00'), datetime('2024-01-01 24:00'), datetime('2024-01-01 23:60'), datetime('2024-01-01 23:59:60'), time('24:00:01');
SELECT date('2024-02-30'), date('2024-13-01'), date('2024-00-10'), date('2024-01-00'), date('2024-01-32');
SELECT datetime('2024-01-01t10:00'), datetime('2024-01-01T10:00'), datetime('2024-01-01 10:00:00+15:00'), datetime('2024-01-01 10:00:00+14:59'), datetime('2024-01-01 10:00 -0130');
SELECT timediff('2024-03-01', '2024-02-01'), timediff('2024-02-01', '2024-03-01'), timediff('2024-03-31', '2024-02-29'), timediff('2025-01-01 00:00:00', '2024-12-31 23:59:59.5');
SELECT timediff('2024-01-31', '2024-03-01'), timediff('2000-03-01', '1999-02-28'), timediff('2024-05-01 10:00', '2024-04-30 12:00'), timediff('2030-06-15 12:34:56.789', '1999-12-31');
SELECT date('2024-02-29', '+1 year'), date('2024-01-31', '+1 month'), datetime('2024-03-10 12:30:00', '-90 minutes'), strftime('%Y-%j %H:%M:%f %w %W %s', '2024-07-04 05:06:07.891');
-- COMMIT / END without an open transaction is an error.
COMMIT;
END;
