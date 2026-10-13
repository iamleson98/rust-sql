-- WITH RECURSIVE follows generateWithRecursiveQuery: setup rows seed a
-- queue; each row taken off it is output (after the compound's OFFSET,
-- until its LIMIT) and then fed alone through the recursive arm(s). The
-- compound's ORDER BY orders the queue. The LIMIT / OFFSET / ORDER BY
-- used to be copied onto BOTH arms: `LIMIT 50000` ran to the 1,000,000
-- iteration cap, `LIMIT 5 OFFSET 2` returned nothing.
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t LIMIT 5) SELECT x FROM t;
SELECT count(*), max(x) FROM (WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t LIMIT 50000) SELECT x FROM t);
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t LIMIT 5 OFFSET 2) SELECT x FROM t;
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 5 UNION ALL SELECT x - 1 FROM t WHERE x > 0 ORDER BY 1 LIMIT 3) SELECT x FROM t;
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 5 UNION ALL SELECT x - 1 FROM t WHERE x > 0 ORDER BY 1 DESC) SELECT x FROM t;
/*ordered*/ WITH RECURSIVE t(n, p) AS (SELECT 1, NULL UNION ALL SELECT n + 1, n FROM t WHERE n < 6 ORDER BY 2 DESC LIMIT 4) SELECT n, p FROM t;
/*ordered*/ WITH RECURSIVE t(n, p) AS (SELECT 1, NULL UNION ALL SELECT n + 1, n FROM t WHERE n < 6 ORDER BY p NULLS LAST) SELECT n, p FROM t;
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t WHERE x < 3 LIMIT 0) SELECT x FROM t;
/*ordered*/ WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t WHERE x < 4 LIMIT -1 OFFSET 1) SELECT x FROM t;
-- several recursive arms, UNION and UNION ALL, setup compounds
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4 UNION ALL SELECT x * 10 FROM c WHERE x < 3) SELECT x FROM c;
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4 UNION ALL SELECT x * 10 FROM c WHERE x < 3 LIMIT 6) SELECT x FROM c;
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 1 UNION SELECT x + 1 FROM c WHERE x < 4 UNION SELECT x - 1 FROM c WHERE x > -2) SELECT x FROM c;
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 2 UNION ALL SELECT 1 UNION SELECT x + 1 FROM c WHERE x < 3) SELECT x FROM c;
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 1 UNION SELECT x % 3 + 1 FROM c) SELECT x FROM c;
-- a WITH RECURSIVE CTE that never recurses is an ordinary one
/*ordered*/ WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT 2) SELECT x FROM c;
-- a recursive arm before a non-recursive one, or first: circular
WITH RECURSIVE c(x) AS (SELECT 1 UNION SELECT x + 1.0 FROM c WHERE x < 3 UNION SELECT 5) SELECT x FROM c;
WITH RECURSIVE c(x) AS (SELECT x FROM c UNION ALL SELECT 1) SELECT x FROM c;
-- hierarchies: joins in the recursive arm, LIMIT and ORDER BY
CREATE TABLE tree(id INTEGER PRIMARY KEY, parent INT, name TEXT);
INSERT INTO tree VALUES (1, NULL, 'root'), (2, 1, 'a'), (3, 1, 'b'), (4, 2, 'c'), (5, 2, 'd'), (6, 3, 'e'), (7, 4, 'f');
WITH RECURSIVE sub(id, depth) AS (SELECT 1, 0 UNION ALL SELECT tree.id, depth + 1 FROM tree JOIN sub ON tree.parent = sub.id) SELECT id, depth FROM sub;
/*ordered*/ WITH RECURSIVE sub(id, depth) AS (SELECT 1, 0 UNION ALL SELECT tree.id, depth + 1 FROM tree JOIN sub ON tree.parent = sub.id LIMIT 4) SELECT id, depth FROM sub;
/*ordered*/ WITH RECURSIVE sub(id, depth) AS (SELECT 1, 0 UNION ALL SELECT tree.id, depth + 1 FROM tree, sub WHERE tree.parent = sub.id ORDER BY 2 DESC LIMIT 5) SELECT id, depth FROM sub;
/*ordered*/ WITH RECURSIVE path(id, p) AS (SELECT id, name FROM tree WHERE parent IS NULL UNION ALL SELECT tree.id, p || '/' || tree.name FROM tree JOIN path ON tree.parent = path.id ORDER BY 2) SELECT p FROM path;
