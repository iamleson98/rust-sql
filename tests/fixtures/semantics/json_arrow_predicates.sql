-- `->` / `->>` inside compiled WHERE filters, arithmetic, GROUP BY keys,
-- aggregate arguments and UPDATE assignments. The compiled operand trees
-- evaluated the arrows as NULL: `WHERE x ->> 'f' > 50` matched nothing.
CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT, y);
WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i + 1 FROM n WHERE i < 60)
INSERT INTO t SELECT i, json_object('f', i, 'b', 'v' || (i % 10), 'g', i % 4, 'r', i * 0.5), i FROM n;
SELECT count(*) FROM t WHERE x ->> 'f' > 50;
SELECT count(*) FROM t WHERE x ->> '$.b' > 'v5';
SELECT count(*) FROM t WHERE (x ->> 'f') + 1 > 50 AND x -> 'g' = '1';
SELECT count(*) FROM t WHERE x ->> 'f' BETWEEN 10 AND 20;
SELECT count(*) FROM t WHERE x ->> 'f' % 7 = 0;
SELECT count(*) FROM t WHERE x ->> 'r' * 2 = id;
SELECT count(*), sum(id) FROM t WHERE x ->> 'f' IN (3, 5, 70);
/*ordered*/ SELECT x ->> 'g' AS g, count(*), sum(x ->> 'f') FROM t GROUP BY x ->> 'g' ORDER BY g;
/*ordered*/ SELECT (x ->> 'f') / 10 AS b, count(*) FROM t GROUP BY b ORDER BY b;
SELECT sum(x ->> 'f' + 1), max(x -> 'b'), min(x ->> 'r') FROM t;
UPDATE t SET y = x ->> 'f' * 2 WHERE x ->> 'g' = 0;
SELECT count(*), sum(y) FROM t WHERE y = (x ->> 'f') * 2;
UPDATE t SET y = x -> 'b';
/*ordered*/ SELECT y, count(*) FROM t GROUP BY y ORDER BY y;
DELETE FROM t WHERE x ->> 'f' > 55;
SELECT count(*) FROM t;
