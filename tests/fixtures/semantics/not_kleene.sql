-- Three-valued logic of compiled WHERE predicates under NOT: AND / OR
-- propagate NULL (Kleene), so NOT over an all-NULL OR filters the row.
CREATE TABLE v(x INTEGER, y INTEGER, z INTEGER);
INSERT INTO v VALUES (NULL, NULL, NULL), (NULL, NULL, 0), (NULL, NULL, 1), (NULL, 0, NULL), (NULL, 0, 0), (NULL, 0, 1), (NULL, 1, NULL), (NULL, 1, 0), (NULL, 1, 1), (0, NULL, NULL), (0, NULL, 0), (0, NULL, 1), (0, 0, NULL), (0, 0, 0), (0, 0, 1), (0, 1, NULL), (0, 1, 0), (0, 1, 1), (1, NULL, NULL), (1, NULL, 0), (1, NULL, 1), (1, 0, NULL), (1, 0, 0), (1, 0, 1), (1, 1, NULL), (1, 1, 0), (1, 1, 1);
SELECT x, y, z FROM v WHERE NOT (x = 1 AND y = 1);
SELECT x, y, z FROM v WHERE NOT (x = 1 OR y = 1);
SELECT x, y, z FROM v WHERE NOT (NOT (x = 1) OR y = 1);
SELECT x, y, z FROM v WHERE NOT (x = 1 AND (y = 1 OR z = 1));
SELECT x, y, z FROM v WHERE NOT ((x = 1 OR y = 1) AND z = 1);
SELECT x, y, z FROM v WHERE NOT (x IN (1, NULL) OR y IN (0, NULL));
SELECT x, y, z FROM v WHERE NOT (x IN (1, NULL) AND y = 1);
SELECT x, y, z FROM v WHERE NOT (x BETWEEN 0 AND 1 OR y > 0);
SELECT x, y, z FROM v WHERE NOT (x IS NULL OR y = 1);
SELECT x, y, z FROM v WHERE NOT (x LIKE '1' OR y = 0);
SELECT x, y, z FROM v WHERE NOT (NOT (x = 1 AND y = 0));
SELECT x, y, z FROM v WHERE NOT (x NOT IN (0, NULL) OR y = 1);
SELECT x, y, z FROM v WHERE x = 1 OR NOT (y = 1 AND z = 1);
SELECT x, y, z FROM v WHERE NOT (x = 1 OR y = 1) OR z = 1;
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER);
INSERT INTO t2(g) VALUES ('0'), (-1.25), (NULL), (x'41'), (NULL);
SELECT id FROM t2 WHERE NOT ((id IN (SELECT g FROM t2)) OR (g IN (SELECT g FROM t2)));
SELECT id FROM t2 WHERE NOT (id IN (SELECT g FROM t2) OR g > 100);
SELECT id FROM t2 WHERE NOT (g IN (0, NULL) OR g IN (-1.25, NULL));
