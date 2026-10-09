-- A column's DECLARED collation applies to every comparison context
-- (SELECT list, CASE, HAVING, nested comparisons, IN / BETWEEN / IS),
-- to min()/max(), to DISTINCT (plain and inside aggregates) and to the
-- compound operators — not only to top-level WHERE / ON comparisons.
CREATE TABLE t(a INTEGER, b TEXT COLLATE NOCASE, c TEXT);
INSERT INTO t VALUES(1,'a','a'),(2,'B','B'),(3,'A','Z'),(4,'b','b'),(5,'c','C');
SELECT a, b = 'A', b < 'B', 'A' = b, b IN ('A'), b BETWEEN 'A' AND 'A', 'b' = b, b > 'b' FROM t;
SELECT a, +b = 'A', CAST(b AS TEXT) = 'A', b || '' = 'A', upper(b) = 'a', coalesce(b, '') = 'A' FROM t;
SELECT a, c = b, b = c, c IN (b), c BETWEEN b AND b, 'A' BETWEEN b AND b, 'A' BETWEEN c AND b FROM t;
SELECT a, (c COLLATE NOCASE) = 'z', upper(c COLLATE NOCASE) = 'z', c = 'z' COLLATE NOCASE FROM t;
SELECT a, CASE WHEN b = 'A' THEN 1 ELSE 0 END, b IS 'A', b IS NOT 'A' FROM t;
SELECT a FROM t WHERE (b = 'A') = 1;
SELECT a FROM t WHERE coalesce(b = 'A', 0);
SELECT b, count(*) FROM t GROUP BY b HAVING b = 'A';
SELECT t1.a FROM t t1 JOIN t t2 ON t1.c = t2.b WHERE t1.a = t2.a;
SELECT max(b), min(b) FROM t;
SELECT max(b, 'B'), min('C', b) FROM t;
SELECT count(DISTINCT b), count(DISTINCT c), count(DISTINCT b COLLATE BINARY) FROM t;
SELECT group_concat(DISTINCT b) FROM t;
SELECT a % 2, count(DISTINCT b), min(b), max(b) FROM t GROUP BY a % 2;
SELECT min(b COLLATE BINARY), max(c COLLATE NOCASE) FROM t;
SELECT DISTINCT b FROM t;
SELECT DISTINCT b, a % 2 FROM t;
SELECT DISTINCT b COLLATE BINARY FROM t;
SELECT DISTINCT upper(b) FROM t;
SELECT DISTINCT t1.b FROM t t1 JOIN t t2 ON t1.a = t2.a;
SELECT count(*) FROM (SELECT b FROM t UNION SELECT 'C');
SELECT count(*) FROM (SELECT 'C' UNION SELECT b FROM t);
SELECT b FROM t INTERSECT SELECT 'A';
SELECT b FROM t EXCEPT SELECT 'A';
SELECT count(*) FROM (SELECT b FROM t UNION ALL SELECT 'C');
-- min() ties pick the LATER argument, max() the earlier (minmaxFunc).
SELECT min(1, 1.0), typeof(min(1, 1.0)), max(1, 1.0), typeof(max(1, 1.0)), min(1.0, 1), typeof(min(1.0, 1));
SELECT min(2, 1, 1.0), typeof(min(2, 1, 1.0)), max(2.0, 2, 1), typeof(max(2.0, 2, 1));
