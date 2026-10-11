-- SQLite splits a WHERE into its top-level AND terms and codes each, in order,
-- as a jump-if-false-or-NULL; terms holding a correlated subquery go last. The
-- right-operand-first rule of an AND expression applies inside a term (an OR
-- arm, a CASE WHEN), not across terms: a NULL first term rejects the row
-- before `abs(i64::MIN)` runs (strict-fuzz seed 745212).
CREATE TABLE t1(a INTEGER, b);
CREATE TABLE t2(g INTEGER);
INSERT INTO t1 VALUES (1, 'x'), (-9223372036854775808, 'y'), (3, NULL);
INSERT INTO t2 VALUES (NULL), (5);
SELECT a FROM t1 WHERE ((-9223372036854775808 IN (SELECT g FROM t2)) != 11) AND abs(a);
SELECT a FROM t1 WHERE abs(a) AND ((-9223372036854775808 IN (SELECT g FROM t2)) != 11);
SELECT a FROM t1 WHERE ((SELECT g FROM t2 WHERE g = t1.a) != 11) AND abs(a);
SELECT a FROM t1 WHERE b IS NULL AND abs(a);
SELECT a FROM t1 WHERE abs(a) AND b IS NULL;
SELECT a FROM t1 WHERE b = 'q' AND (SELECT g FROM t2 WHERE g = t1.a) AND abs(a);
SELECT a FROM t1 WHERE (((-9223372036854775808 IN (SELECT g FROM t2)) != 11) AND abs(a)) OR b = 'zz';
SELECT a FROM t1 WHERE NOT (abs(a) OR ((-9223372036854775808 IN (SELECT g FROM t2)) = 11));
SELECT CASE WHEN ((-9223372036854775808 IN (SELECT g FROM t2)) != 11) AND abs(a) THEN 1 ELSE 0 END FROM t1;
SELECT a FROM t1 JOIN t2 ON t2.g IS NULL AND abs(t1.a) WHERE b = 'none';
SELECT a FROM t1 WHERE abs(a) AND 0;
SELECT a FROM t1 WHERE abs(a) AND b = 'x' AND 0;
SELECT a FROM t1 WHERE 1 AND abs(a);
SELECT a FROM t1 WHERE abs(a) AND 1;
SELECT a FROM t1 WHERE b = 'q' AND abs(a) AND 1;
