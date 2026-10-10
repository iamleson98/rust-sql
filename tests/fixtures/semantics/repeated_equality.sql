-- Two equalities on the same indexed column: the index seeks with ONE of
-- them and the other must survive as a residual filter. It used to be
-- dropped with the key's column — `g = 0.0 AND g = 5` returned the g = 0
-- rows.
CREATE TABLE t(id INTEGER PRIMARY KEY, g INTEGER, h TEXT COLLATE NOCASE, k INTEGER);
CREATE INDEX t_g ON t(g);
CREATE INDEX t_h ON t(h);
CREATE INDEX t_gk ON t(g, k);
INSERT INTO t VALUES (1, 0, 'a', 1), (2, 0, 'A', 2), (3, 5, 'b', 1), (4, 5, 'B', 5), (5, NULL, NULL, NULL), (6, 0, 'c', 1);
SELECT id FROM t WHERE g = 0.0 AND g = 5 ORDER BY id;
SELECT id FROM t WHERE 0.0 = g AND g = x'41' ORDER BY id;
SELECT id FROM t WHERE g = 0 AND g = 0.0 ORDER BY id;
SELECT id FROM t WHERE g = 5 AND g = '5' ORDER BY id;
SELECT id FROM t WHERE g = 0 AND k = 1 AND g = 5 ORDER BY id;
SELECT id FROM t WHERE g = 0 AND k = 1 AND k = 2 ORDER BY id;
SELECT id FROM t WHERE g = 0 AND k = 1 AND k = 1 ORDER BY id;
SELECT id FROM t WHERE h = 'a' AND h = 'b' ORDER BY id;
SELECT id FROM t WHERE h = 'a' AND h = 'A' ORDER BY id;
SELECT id FROM t WHERE h = 'a' AND h = 'A' COLLATE BINARY ORDER BY id;
SELECT id FROM t WHERE g = 0 AND g IN (5, 6) ORDER BY id;
SELECT id FROM t WHERE g = 0 AND g = NULL ORDER BY id;
SELECT id FROM t WHERE id = 1 AND id = 2;
SELECT id FROM t WHERE rowid = 2 AND id = 2;
SELECT count(*) FROM t WHERE g = 0 AND g = g + 0 AND g = 5;
UPDATE t SET k = 99 WHERE g = 0 AND g = 5;
SELECT id, k FROM t ORDER BY id;
DELETE FROM t WHERE g = 5 AND g = 0;
SELECT count(*) FROM t;
DELETE FROM t WHERE g = 0 AND k = 1 AND k = 2;
SELECT count(*) FROM t;
