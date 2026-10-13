-- NATURAL and USING joins (sqlite3ProcessJoin). NATURAL JOIN used to run
-- as a cross product; `*` repeated the merged column; `a.*` in a join
-- listed every table's columns; RIGHT / FULL USING's bare column is
-- coalesce(left, right); and NATURAL LEFT JOIN, `FROM a, b USING (k)`,
-- `FROM a, b ON ...` and joins after a NATURAL join did not parse. Also:
-- for equal rows (`1` / `1.0`, NOCASE 'a' / 'A') UNION keeps the right
-- arm's row (multiSelectByMerge).
CREATE TABLE a(k, x);
CREATE TABLE b(k, y);
INSERT INTO a VALUES (1,'a1'),(2,'a2');
INSERT INTO b VALUES (1,'b1'),(3,'b3');
SELECT * FROM a NATURAL JOIN b;
SELECT * FROM a JOIN b USING (k);
SELECT * FROM a LEFT JOIN b USING (k);
SELECT * FROM a NATURAL LEFT JOIN b;
SELECT * FROM a, b USING (k);
SELECT * FROM a, b ON a.k = b.k;
SELECT * FROM a NATURAL JOIN b JOIN a c ON c.k = b.k;
SELECT * FROM a RIGHT JOIN b USING (k);
SELECT * FROM a FULL JOIN b USING (k);
SELECT k FROM a FULL JOIN b USING (k);
SELECT a.*, b.* FROM a JOIN b USING (k);
SELECT * FROM a JOIN b USING (k) JOIN a c USING (k);
SELECT * FROM (SELECT k, x FROM a) s NATURAL JOIN b;
SELECT * FROM a NATURAL JOIN (SELECT 1 AS z);
SELECT count(*) FROM a NATURAL JOIN b;
SELECT * FROM a JOIN b USING (zz);
SELECT * FROM a NATURAL JOIN b ON a.k = b.k;
SELECT * FROM a NATURAL CROSS JOIN b;
SELECT 1 UNION SELECT 1.0;
SELECT 1.0 UNION SELECT 1;
SELECT 1 UNION SELECT 1.0 UNION SELECT 1;
SELECT 1.0 AS x UNION ALL SELECT 2 UNION SELECT 1;
SELECT 'a' COLLATE NOCASE UNION SELECT 'A' UNION SELECT 'b' UNION SELECT 'B';
SELECT 1 INTERSECT SELECT 1.0;
SELECT 1.0 INTERSECT SELECT 1;
SELECT 1.0 EXCEPT SELECT 2;
SELECT 1 UNION SELECT 1.0 ORDER BY 1;
