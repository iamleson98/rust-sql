-- sqlite3ExprSimplifiedAndOr: an AND / OR operand that is an integer literal
-- (or TRUE / FALSE) decides in every context, and the other operand is never
-- evaluated: abs(-9223372036854775808) AND 0 is 0, not an overflow.
CREATE TABLE t(x INTEGER, f INTEGER);
INSERT INTO t VALUES (0, -9223372036854775808), (1, 5), (NULL, 2);
SELECT abs(-9223372036854775808) AND 0;
SELECT abs(-9223372036854775808) OR 1;
SELECT 0 AND abs(-9223372036854775808);
SELECT 1 OR abs(-9223372036854775808);
SELECT (abs(f) AND 0) FROM t;
SELECT (abs(f) OR 7) FROM t;
SELECT x, (x AND 1), (x OR 0), (x AND 0), (x OR 1), (1 AND x), (0 OR x) FROM t;
SELECT x, ((x AND 0) OR 1), ((abs(f) AND FALSE) OR x) FROM t;
SELECT x FROM t WHERE x = 1 OR (abs(f) AND 0);
SELECT x FROM t WHERE (abs(f) OR 1) AND x IS NOT NULL;
SELECT CASE WHEN abs(f) AND 0 THEN 'y' ELSE 'n' END FROM t;
SELECT x, NULL AND 0, NULL OR 1, 'abc' AND 1, 2.5 AND 1, x'00' OR 0 FROM t;
SELECT count(*) FROM t WHERE abs(f) > 0 AND 0;
SELECT sum(abs(f) AND 0) FROM t;
