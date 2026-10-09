-- Sort keys are evaluated ONCE per row: ORDER BY random() used to make
-- the comparator inconsistent (Rust's sort panics on that), and a sort
-- key that raises must abort the statement.
CREATE TABLE t(a INTEGER, b TEXT);
INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z'),(4,NULL),(5,'x');
SELECT count(*) FROM (SELECT a FROM t ORDER BY random());
SELECT count(*) FROM (SELECT a, b FROM t ORDER BY random(), a);
SELECT count(*) FROM (SELECT a FROM t ORDER BY random() LIMIT 3);
SELECT count(*) FROM (SELECT a, row_number() OVER (ORDER BY random()) FROM t);
SELECT a FROM t ORDER BY abs(-9223372036854775807-1) LIMIT 1;
SELECT a FROM t ORDER BY a, abs(-9223372036854775807-1) LIMIT 1;
SELECT a FROM t ORDER BY abs(-9223372036854775807 - 2 + a);
/*ordered*/ SELECT a, b FROM t ORDER BY b DESC, a;
/*ordered*/ SELECT a FROM t ORDER BY b COLLATE NOCASE, a DESC;
-- Errors inside aggregation (arguments, FILTER, GROUP BY keys, WHERE)
-- are raised, not swallowed as NULL / "row filtered out".
SELECT sum(abs(-9223372036854775807 - 1 + a)) FROM t;
SELECT count(*) FROM t WHERE abs(-9223372036854775807 - 1 + a) > 0;
SELECT count(*) FILTER (WHERE abs(-9223372036854775807 - 1 + a) > 0) FROM t;
SELECT sum(a) FROM t GROUP BY abs(-9223372036854775807 - 1);
SELECT b, sum(abs(-9223372036854775807 - 1 + a)) FROM t GROUP BY b;
SELECT b, count(*) FILTER (WHERE abs(-9223372036854775807 - 1 + a) > 0) FROM t GROUP BY b;
SELECT b, count(*) FROM t WHERE abs(-9223372036854775807 - 1 + a) > 0 GROUP BY b;
SELECT b, count(*) FROM (SELECT * FROM t LIMIT 10) GROUP BY abs(-9223372036854775807 - 1 + a);
