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

-- The other access paths (IN lists, rowid, ranges) with repeated or
-- overlapping constraints on one column.
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, k INTEGER);
CREATE INDEX t2_g ON t2(g);
CREATE INDEX t2_gk ON t2(g, k);
INSERT INTO t2 VALUES (1, 0, 1), (2, 0, 2), (3, 5, 1), (4, 5, 5), (5, NULL, NULL), (6, 7, 1), (7, 9, 9);
SELECT id FROM t2 WHERE g IN (0, 5) AND g IN (5, 7) ORDER BY id;
SELECT id FROM t2 WHERE g IN (0, 5) AND g = 7 ORDER BY id;
SELECT id FROM t2 WHERE g = 7 AND g IN (0, 5) ORDER BY id;
SELECT id FROM t2 WHERE id IN (1, 2, 3) AND id IN (3, 4) ORDER BY id;
SELECT id FROM t2 WHERE id IN (1, 2, 3) AND id = 4 ORDER BY id;
SELECT id FROM t2 WHERE id = 4 AND id IN (1, 2, 3) ORDER BY id;
SELECT id FROM t2 WHERE id > 1 AND id > 3 ORDER BY id;
SELECT id FROM t2 WHERE id < 6 AND id < 3 ORDER BY id;
SELECT id FROM t2 WHERE id BETWEEN 1 AND 6 AND id BETWEEN 4 AND 9 ORDER BY id;
SELECT id FROM t2 WHERE g > 0 AND g > 6 ORDER BY id;
SELECT id FROM t2 WHERE g < 9 AND g < 5 ORDER BY id;
SELECT id FROM t2 WHERE g BETWEEN 0 AND 7 AND g BETWEEN 5 AND 9 ORDER BY id;
SELECT id FROM t2 WHERE g >= 5 AND g = 0 ORDER BY id;
SELECT id FROM t2 WHERE g = 5 AND g > 5 ORDER BY id;
SELECT id FROM t2 WHERE g = 5 AND k IN (1, 5) AND k IN (5, 9) ORDER BY id;
SELECT id FROM t2 WHERE g = 5 AND k = 1 AND k > 1 ORDER BY id;
SELECT id FROM t2 WHERE id = 3 AND id > 3;
SELECT id FROM t2 WHERE rowid IN (1, 3) AND id IN (3, 5);
SELECT id FROM t2 WHERE g IN (SELECT 5) AND g IN (SELECT 7);
SELECT id FROM t2 WHERE g LIKE '5' AND g = 7;
