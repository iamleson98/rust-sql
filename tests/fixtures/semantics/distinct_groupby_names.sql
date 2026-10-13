-- Found by the sqllogictest corpus (random/groupby, evidence/aggfunc).
-- SELECT DISTINCT over an aggregate / GROUP BY query returned one row
-- per group: the aggregate planning path never added the DISTINCT.
-- GROUP BY and HAVING names resolve to a FROM column FIRST and to an
-- output alias only when no column has the name (resolveOrderGroupBy):
-- `SELECT -b AS a ... GROUP BY b, a` grouped by -b. A DISTINCT aggregate
-- takes exactly one argument; `f(DISTINCT *)` is a syntax error.
CREATE TABLE t(a, b);
INSERT INTO t VALUES (1,1),(2,1),(3,2);
SELECT DISTINCT 5 FROM t GROUP BY a;
SELECT DISTINCT b FROM t GROUP BY a;
SELECT DISTINCT b+0 FROM t GROUP BY a;
SELECT DISTINCT count(*) FROM t GROUP BY a;
SELECT DISTINCT 5 AS x FROM t GROUP BY a ORDER BY 1;
SELECT DISTINCT -5 FROM t GROUP BY a HAVING a > 0;
SELECT DISTINCT 5, 6 FROM t GROUP BY a;
CREATE TABLE tab1(col0 INTEGER, col1 INTEGER, col2 INTEGER);
INSERT INTO tab1 VALUES(51,14,96),(85,5,59),(91,47,68),(51,14,8);
SELECT ALL - col2 col1 FROM tab1 AS cor0 GROUP BY col2, col1;
SELECT 65 * col2 * + cor0.col1 AS col1 FROM tab1 AS cor0 GROUP BY col2, col1;
SELECT ( + 21 ) AS col1 FROM tab1 cor0 GROUP BY col2, col1;
SELECT - col2 AS col1 FROM tab1 GROUP BY col2 HAVING NOT NULL BETWEEN + col2 AND AVG ( ALL col1 ) + - col1;
SELECT ALL col0 AS col1 FROM tab1 GROUP BY col0, col0 HAVING + AVG ( + col1 ) IS NULL;
SELECT col0 + 1 AS col1, count(*) FROM tab1 GROUP BY col1;
SELECT col0 + 1 AS g, count(*) FROM tab1 GROUP BY g;
SELECT col0 AS col2, sum(col1) FROM tab1 GROUP BY col0 HAVING col2 > 60;
SELECT col0 AS x, sum(col1) FROM tab1 GROUP BY col0 HAVING x > 60;
SELECT count(DISTINCT *) FROM t1;
SELECT group_concat(DISTINCT x, ':') FROM t1;
SELECT max(DISTINCT x, 1) FROM t1;
SELECT string_agg(DISTINCT x, ',') FROM t1;
SELECT count(DISTINCT x, y) FROM t1;
SELECT sum(DISTINCT) FROM t1;
SELECT count(DISTINCT) FROM t1;
SELECT abs(DISTINCT x) FROM t1;
SELECT json_group_array(DISTINCT x) FROM t1;
SELECT group_concat(DISTINCT x) FROM t1;
