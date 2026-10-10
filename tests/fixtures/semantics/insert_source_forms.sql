-- The INSERT source is a full select statement in SQLite's grammar: it
-- may carry its own WITH clause (visible only inside it) and a VALUES
-- list may continue as a compound. Right after a FROM clause `ON` is a
-- join constraint, so an upsert there is a syntax error (`WHERE true`
-- is SQLite's documented fix).
CREATE TABLE a(x INTEGER PRIMARY KEY, p TEXT);
INSERT INTO a WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < 5) SELECT v, 'p' || v FROM s;
INSERT INTO a(x) WITH c AS (SELECT 9) SELECT * FROM c;
INSERT INTO a WITH c AS (SELECT 10, 'z') VALUES (11, 'w');
REPLACE INTO a WITH c AS (SELECT 12, 'y') SELECT * FROM c;
INSERT INTO a VALUES (13, 'a') UNION SELECT 14, 'b';
INSERT INTO a VALUES (15, 'c') UNION ALL VALUES (16, 'd');
INSERT INTO a SELECT 17, 'e' UNION SELECT 18, 'f' ORDER BY 1 LIMIT 1;
WITH c(v) AS (VALUES (20)) INSERT INTO a WITH d(w) AS (SELECT v FROM c) SELECT w, 'h' FROM d;
INSERT INTO a WITH c(v) AS (VALUES (7)) SELECT v, 'g' FROM c ON CONFLICT DO NOTHING;
INSERT INTO a SELECT x + 100, p FROM a ON CONFLICT(x) DO UPDATE SET p = 'u';
INSERT INTO a SELECT x, p FROM a WHERE true ON CONFLICT(x) DO UPDATE SET p = 'u' || excluded.p;
INSERT INTO a SELECT x + 200, p FROM a ORDER BY x LIMIT 2 ON CONFLICT DO NOTHING;
INSERT INTO a SELECT 300, 'q' ON CONFLICT DO NOTHING;
INSERT INTO a SELECT x + 400, 'r' FROM a GROUP BY x ON CONFLICT DO NOTHING;
SELECT x, p FROM a ORDER BY x;
SELECT count(*) FROM a;
