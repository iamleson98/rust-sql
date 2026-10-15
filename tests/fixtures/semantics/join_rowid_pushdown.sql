-- A rowid constraint on one side of a join pushes into that side as a
-- rowid seek; results must match the unpushed filter in every join shape.
CREATE TABLE a(x, y);
CREATE TABLE b(p, q);
CREATE TABLE w(k INTEGER PRIMARY KEY, z) WITHOUT ROWID;
INSERT INTO a VALUES (1, 'a1'), (2, 'a2'), (3, 'a3');
INSERT INTO b VALUES (1, 'b1'), (2, 'b2'), (3, 'b3'), (4, 'b4');
INSERT INTO w VALUES (1, 'w1'), (2, 'w2');
/*ordered*/ SELECT x, p FROM a, b WHERE b.rowid = 2 ORDER BY x;
/*ordered*/ SELECT x, p FROM a, b WHERE b.oid = 3 AND a._rowid_ >= 2 ORDER BY x;
/*ordered*/ SELECT a1.x, a2.x FROM a a1, a a2 WHERE a2.rowid = 1 AND a1.rowid = a2.rowid + 1;
/*ordered*/ SELECT x, q FROM a LEFT JOIN b ON b.p = a.x WHERE b.rowid = 2 ORDER BY x;
/*ordered*/ SELECT x, q FROM a LEFT JOIN b ON b.p = a.x AND b.rowid = 2 ORDER BY x;
/*ordered*/ SELECT x, q FROM a LEFT JOIN b ON b.p = a.x WHERE b.rowid IS NULL ORDER BY x;
/*ordered*/ SELECT x, z FROM a, w WHERE w.k = 2 ORDER BY x;
WITH c(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM c WHERE n < 5)
SELECT n, y FROM c, a WHERE a.rowid = 2 ORDER BY n;
WITH c(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM c WHERE n < 5)
SELECT y, count(*) FROM c, a WHERE a.rowid = n % 3 GROUP BY y ORDER BY y;
